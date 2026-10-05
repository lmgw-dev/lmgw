//! `GET /v1/realtime` over a real socket (realtime design §10, §16 "Route
//! plumbing" and "Session state machine").
//!
//! A `tokio-tungstenite` client against the real router: the handshake's
//! refusals (426, beta, cross-origin, unknown model, key policy), the
//! subprotocol credential and its path scoping, model resolution and its
//! echoes, `session.created` / `session.update`, the error path that keeps a
//! session open, the concurrency slot a session holds, and the explicit size
//! limit. No model is ever called here; responses are `realtime_response`'s.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use lmgw_core::config::{hash_api_key, KeyPolicy, ScopeMode, Settings, DEFAULT_VOICE_INSTRUCTIONS};
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderName;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const KEY: &str = "lmgw-realtime-test-key";

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A gateway with two chat aliases (`chatty`, `other`) on an upstream that is
/// never called, settings adjusted by `tweak`, and — when `policy` is given —
/// one client key carrying it.
async fn gateway(
    auth: bool,
    policy: Option<KeyPolicy>,
    tweak: impl FnOnce(&mut Settings),
) -> (SharedState, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = auth;
    tweak(&mut settings);
    lmgw_core::store::save_settings(&state.db, &settings)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url, extra_headers, timeout_ms,
             enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses)
         VALUES ('nowhere', 'openai', 'generic', 'http://127.0.0.1:9', '[]', 1000, 1, 0, '',
                 datetime('now'), datetime('now'), 0)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    for alias in ["chatty", "other"] {
        sqlx::query(
            "INSERT INTO models (alias, upstream_id, upstream_model_id, param_overrides, enabled,
                 created_at, updated_at)
             VALUES (?1, (SELECT id FROM upstreams WHERE name='nowhere'), 'm', '{}', 1,
                     datetime('now'), datetime('now'))",
        )
        .bind(alias)
        .execute(&state.db)
        .await
        .unwrap();
    }
    if let Some(p) = policy {
        sqlx::query(
            "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
                 budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit)
             VALUES ('voice', ?1, 1, ?2, ?3, 0, ?4, ?5, 0, ?6)",
        )
        .bind(hash_api_key(KEY))
        .bind(p.scope_mode.as_str())
        .bind(&p.scope_patterns)
        .bind(p.budget_period.as_str())
        .bind(p.rpm_limit)
        .bind(p.concurrency_limit)
        .execute(&state.db)
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, addr.to_string())
}

/// The handshake with extra headers: the socket, or the HTTP status and JSON
/// body of the refusal.
async fn connect(
    addr: &str,
    path_and_query: &str,
    headers: &[(&str, &str)],
) -> Result<(Ws, Option<String>), (u16, Value)> {
    let mut req = format!("ws://{addr}{path_and_query}")
        .into_client_request()
        .unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().expect("a header-safe test value"),
        );
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, resp)) => {
            let proto = resp
                .headers()
                .get("sec-websocket-protocol")
                .map(|v| v.to_str().unwrap().to_string());
            Ok((ws, proto))
        }
        Err(WsError::Http(resp)) => {
            let status = resp.status().as_u16();
            let body = resp
                .body()
                .as_deref()
                .and_then(|b| serde_json::from_slice(b).ok())
                .unwrap_or(Value::Null);
            Err((status, body))
        }
        Err(e) => panic!("handshake failed below HTTP: {e}"),
    }
}

/// A handshake that must succeed: the socket and the selected subprotocol.
async fn upgraded(
    addr: &str,
    path_and_query: &str,
    headers: &[(&str, &str)],
) -> (Ws, Option<String>) {
    connect(addr, path_and_query, headers)
        .await
        .unwrap_or_else(|(status, body)| panic!("expected a 101, got {status}: {body}"))
}

async fn open(addr: &str, path_and_query: &str, headers: &[(&str, &str)]) -> Ws {
    upgraded(addr, path_and_query, headers).await.0
}

/// A handshake that must be refused: its status and error body.
async fn refused(addr: &str, path_and_query: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    match connect(addr, path_and_query, headers).await {
        Ok(_) => panic!("expected a refusal before the 101, got the upgrade"),
        Err(refusal) => refusal,
    }
}

/// The next JSON event, failing the test after five seconds of silence.
async fn next_event(ws: &mut Ws) -> Value {
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

async fn send(ws: &mut Ws, event: Value) {
    ws.send(Message::text(event.to_string())).await.unwrap();
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_get_is_426_with_an_openai_error() {
    let (_s, addr) = gateway(false, None, |_| {}).await;
    let resp = reqwest::get(format!("http://{addr}/v1/realtime?model=chatty"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 426);
    assert_eq!(
        resp.headers().get("upgrade").and_then(|v| v.to_str().ok()),
        Some("websocket")
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code(&body), "upgrade_required");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn a_beta_client_is_400_before_the_upgrade() {
    let (_s, addr) = gateway(false, None, |_| {}).await;
    let (status, body) = refused(
        &addr,
        "/v1/realtime?model=chatty",
        &[("openai-beta", "realtime=v1")],
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(code(&body), "beta_protocol_unsupported");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("GA Realtime protocol"));
}

#[tokio::test]
async fn the_realtime_subprotocol_is_selected_only_when_offered() {
    let (_s, addr) = gateway(false, None, |_| {}).await;
    // The stock SDKs offer none at all, and must still get their 101.
    let (_ws, proto) = upgraded(&addr, "/v1/realtime?model=chatty", &[]).await;
    assert_eq!(proto, None);
    let (_ws, proto) = upgraded(
        &addr,
        "/v1/realtime?model=chatty",
        &[("sec-websocket-protocol", "realtime")],
    )
    .await;
    assert_eq!(proto.as_deref(), Some("realtime"));
}

#[tokio::test]
async fn the_subprotocol_key_is_a_credential_on_this_route_only() {
    let (_s, addr) = gateway(true, Some(KeyPolicy::default()), |_| {}).await;
    let offered = format!("realtime, openai-insecure-api-key.{KEY}");

    // With **Require API key** on and nothing else presented, the browser's
    // subprotocol key opens the session — and the 101 selects `realtime`,
    // never echoing the key back.
    let (mut ws, proto) = upgraded(
        &addr,
        "/v1/realtime?model=chatty",
        &[("sec-websocket-protocol", &offered)],
    )
    .await;
    assert_eq!(proto.as_deref(), Some("realtime"));
    assert_eq!(next_event(&mut ws).await["type"], "session.created");

    // A wrong key is no credential.
    let (status, _) = refused(
        &addr,
        "/v1/realtime?model=chatty",
        &[(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.nope",
        )],
    )
    .await;
    assert_eq!(status, 401);

    // The same header on any other route authenticates nothing.
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .header("sec-websocket-protocol", &offered)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn an_anonymous_upgrade_from_a_foreign_origin_is_refused() {
    let (state, addr) = gateway(false, Some(KeyPolicy::default()), |_| {}).await;
    let (status, body) = refused(
        &addr,
        "/v1/realtime?model=chatty",
        &[("origin", "http://evil.example")],
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(code(&body), "cross_origin_refused");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("http://evil.example"));
    let (proto,): (String,) = sqlx::query_as(
        "SELECT ingress_proto FROM request_logs WHERE error_kind = 'cross_origin_refused'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(proto, "realtime");

    // This gateway's own origin, and no origin at all (an SDK), are fine.
    let own = format!("http://{addr}");
    open(&addr, "/v1/realtime?model=chatty", &[("origin", &own)]).await;
    open(&addr, "/v1/realtime?model=chatty", &[]).await;

    // A key makes the request not anonymous, whatever page it came from.
    let bearer = format!("Bearer {KEY}");
    open(
        &addr,
        "/v1/realtime?model=chatty",
        &[
            ("origin", "http://evil.example"),
            ("authorization", &bearer),
        ],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Model resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_model_is_404_before_the_upgrade() {
    let (state, addr) = gateway(false, None, |_| {}).await;
    let (status, body) = refused(&addr, "/v1/realtime?model=nope", &[]).await;
    assert_eq!(status, 404);
    assert_eq!(code(&body), "unknown_alias");
    // Its row names the model asked for.
    let (alias, proto): (String, String) = sqlx::query_as(
        "SELECT requested_alias, ingress_proto FROM request_logs WHERE error_kind = \
         'unknown_alias'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!((alias.as_str(), proto.as_str()), ("nope", "realtime"));

    // An OpenAI Realtime name with no default configured says which setting
    // is missing.
    let (status, body) = refused(&addr, "/v1/realtime?model=gpt-realtime", &[]).await;
    assert_eq!(status, 404);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("realtime.default_model"));
}

#[tokio::test]
async fn openai_names_and_mapped_names_resolve_and_are_echoed() {
    let (_s, addr) = gateway(false, None, |s| {
        s.realtime.default_model = "chatty".into();
        s.realtime
            .model_map
            .insert("my-voice".into(), "other".into());
    })
    .await;

    let mut ws = open(&addr, "/v1/realtime?model=gpt-realtime-2.1", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["model"], "gpt-realtime-2.1");
    assert_eq!(created["session"]["lmgw"]["resolved"]["chat"], "chatty");

    let mut ws = open(&addr, "/v1/realtime?model=my-voice", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["lmgw"]["resolved"]["chat"], "other");

    // A real alias that nothing maps is used as is.
    let mut ws = open(&addr, "/v1/realtime?model=other", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["lmgw"]["resolved"]["chat"], "other");
}

#[tokio::test]
async fn model_map_and_openai_names_come_before_any_alias() {
    let (state, addr) = gateway(false, None, |s| {
        s.realtime.default_model = "chatty".into();
        // The owner's explicit intent wins even over a real alias.
        s.realtime.model_map.insert("chatty".into(), "other".into());
        s.realtime
            .model_map
            .insert("my-voice".into(), "other".into());
    })
    .await;
    // A catch-all upstream, which resolves every name as a chat model.
    sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url, extra_headers, timeout_ms,
             enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses)
         VALUES ('everything', 'openai', 'generic', 'http://127.0.0.1:9', '[]', 1000, 1, 1, '',
                 datetime('now'), datetime('now'), 0)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    for (asked, resolved) in [
        // Mapped, though the passthrough would have taken the name.
        ("my-voice", "other"),
        // Mapped, though it is an alias of its own.
        ("chatty", "other"),
        // Never a passthrough model: a chat endpoint cannot serve it.
        ("gpt-realtime-2.1", "chatty"),
        ("gpt-4o-mini-realtime-preview", "chatty"),
        // Anything else is an alias like any other, the passthrough's too.
        ("some-cloud-model", "some-cloud-model"),
    ] {
        let mut ws = open(&addr, &format!("/v1/realtime?model={asked}"), &[]).await;
        let created = next_event(&mut ws).await;
        assert_eq!(
            created["session"]["lmgw"]["resolved"]["chat"], resolved,
            "{asked}"
        );
    }
}

#[tokio::test]
async fn a_model_less_handshake_starts_on_the_default() {
    let (_s, addr) = gateway(false, None, |s| {
        s.realtime.default_model = "chatty".into();
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["model"], "chatty");
    assert_eq!(created["session"]["lmgw"]["resolved"]["chat"], "chatty");

    // With no default at all the session still opens — `@openai/agents`
    // names its model in the first update — and says nothing resolved.
    let (_s, addr) = gateway(false, None, |_| {}).await;
    let mut ws = open(&addr, "/v1/realtime", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["lmgw"]["resolved"]["chat"], Value::Null);
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime", "model": "chatty"}}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated");
    assert_eq!(updated["session"]["lmgw"]["resolved"]["chat"], "chatty");
}

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_created_comes_first_with_every_default() {
    let (_s, addr) = gateway(false, None, |s| {
        s.realtime.default_instructions = Some("Speak briefly.".into());
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let ev = next_event(&mut ws).await;
    assert_eq!(ev["type"], "session.created");
    assert!(ev["event_id"].as_str().unwrap().starts_with("event_"));
    let s = &ev["session"];
    assert!(s["id"].as_str().unwrap().starts_with("sess_"));
    assert_eq!(s["type"], "realtime");
    assert_eq!(s["object"], "realtime.session");
    assert_eq!(s["model"], "chatty");
    assert_eq!(s["instructions"], "Speak briefly.");
    assert_eq!(s["output_modalities"], json!(["audio"]));
    assert_eq!(s["tools"], json!([]));
    assert_eq!(s["tool_choice"], "auto");
    assert_eq!(
        s["audio"]["input"]["format"],
        json!({"type": "audio/pcm", "rate": 24000})
    );
    assert_eq!(
        s["audio"]["input"]["turn_detection"],
        json!({"type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300,
               "silence_duration_ms": 500, "create_response": true,
               "interrupt_response": true})
    );
    assert_eq!(
        s["audio"]["output"]["format"],
        json!({"type": "audio/pcm", "rate": 24000})
    );
    assert_eq!(s["audio"]["output"]["voice"], "marin");
    assert_eq!(s["lmgw"]["resolved"]["chat"], "chatty");
}

#[tokio::test]
async fn voice_instructions_stand_in_until_the_client_gives_its_own() {
    // §23 L8: with none, the first live answers ran 19 s, with lists and
    // bold text read out.
    let (_s, addr) = gateway(false, None, |_| {}).await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(
        created["session"]["instructions"],
        DEFAULT_VOICE_INSTRUCTIONS
    );
    // The client's own always win; empty ones are none, and the default is
    // what is in effect again — and echoed.
    for (sent, in_effect) in [
        ("Be a pirate.", "Be a pirate."),
        ("", DEFAULT_VOICE_INSTRUCTIONS),
        ("Be a pirate.", "Be a pirate."),
        ("  ", DEFAULT_VOICE_INSTRUCTIONS),
    ] {
        send(
            &mut ws,
            json!({"type": "session.update",
                   "session": {"type": "realtime", "instructions": sent}}),
        )
        .await;
        let u = next_event(&mut ws).await;
        assert_eq!(u["session"]["instructions"], in_effect, "{sent:?}");
    }
    // A text-only session gets none (package B review 8): they ask for
    // answers made to be heard. Switching back to audio brings them back,
    // and a client's own stay through both.
    for (modality, in_effect) in [("text", ""), ("audio", DEFAULT_VOICE_INSTRUCTIONS)] {
        send(
            &mut ws,
            json!({"type": "session.update",
                   "session": {"type": "realtime", "output_modalities": [modality]}}),
        )
        .await;
        let u = next_event(&mut ws).await;
        assert_eq!(u["session"]["instructions"], in_effect, "{modality}");
    }
    // A client that sends back the session it was given — the default text
    // with it — is judged the same (B2 review 6): text output drops them.
    for (modality, in_effect) in [("text", ""), ("audio", DEFAULT_VOICE_INSTRUCTIONS)] {
        send(
            &mut ws,
            json!({"type": "session.update", "session": {"type": "realtime",
                   "instructions": DEFAULT_VOICE_INSTRUCTIONS,
                   "output_modalities": [modality]}}),
        )
        .await;
        let u = next_event(&mut ws).await;
        assert_eq!(u["session"]["instructions"], in_effect, "{modality}");
    }
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "instructions": "Be a pirate.", "output_modalities": ["text"]}}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await["session"]["instructions"],
        "Be a pirate."
    );
    // The owner can turn the default off: an empty setting, not an unset
    // one, which is the built-in default.
    let (_s, addr) = gateway(false, None, |s| {
        s.realtime.default_instructions = Some(String::new())
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    assert_eq!(next_event(&mut ws).await["session"]["instructions"], "");
    for (stored, want) in [
        (json!({}), Some(DEFAULT_VOICE_INSTRUCTIONS)),
        (
            json!({"default_instructions": null}),
            Some(DEFAULT_VOICE_INSTRUCTIONS),
        ),
        (json!({"default_instructions": ""}), None),
        (json!({"default_instructions": "Hi."}), Some("Hi.")),
    ] {
        let r: lmgw_core::config::RealtimeSettings =
            serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(r.voice_instructions(), want, "{stored}");
    }
}

#[tokio::test]
async fn session_update_answers_with_the_whole_session_each_time() {
    let (_s, addr) = gateway(false, None, |s| {
        s.realtime.default_model = "chatty".into();
    })
    .await;
    // `@openai/agents` with an explicit URL: no query, no subprotocol.
    let mut ws = open(&addr, "/v1/realtime", &[]).await;
    let created = next_event(&mut ws).await;

    send(
        &mut ws,
        json!({"type": "session.update", "session": {
            "type": "realtime",
            "model": "gpt-realtime-2.1",
            "instructions": "Be helpful.",
            "output_modalities": ["audio"],
            "audio": {
                "input": {"format": {"type": "audio/pcm", "rate": 24000},
                          "noise_reduction": null,
                          "transcription": {"model": "gpt-4o-mini-transcribe"},
                          "turn_detection": {"type": "semantic_vad"}},
                "output": {"format": {"type": "audio/pcm", "rate": 24000}, "speed": 1}
            },
            "tools": [{"type": "function", "name": "lookup", "parameters": {
                "$schema": "http://json-schema.org/draft-07/schema#", "type": "object",
                "properties": {"q": {"anyOf": [{"type": "string"}, {"type": "null"}]}}}}]
        }}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated");
    assert_ne!(updated["event_id"], created["event_id"]);
    let s = &updated["session"];
    assert_eq!(s["id"], created["session"]["id"]);
    assert_eq!(s["instructions"], "Be helpful.");
    assert_eq!(s["lmgw"]["resolved"]["chat"], "chatty");
    let td = &s["audio"]["input"]["turn_detection"];
    assert_eq!(td["type"], "semantic_vad");
    assert_eq!(td["create_response"], true);
    assert_eq!(td["interrupt_response"], true);
    assert_eq!(s["tools"][0]["name"], "lookup");

    // The follow-up `{type, tracing}` gets its own `session.updated`.
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime", "tracing": "auto"}}),
    )
    .await;
    let again = next_event(&mut ws).await;
    assert_eq!(again["type"], "session.updated");
    assert_eq!(again["session"]["tracing"], "auto");
    assert_eq!(again["session"]["instructions"], "Be helpful.");
}

#[tokio::test]
async fn a_bad_event_is_an_error_and_the_session_goes_on() {
    let (_s, addr) = gateway(false, None, |_| {}).await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;

    ws.send(Message::text("this is not json")).await.unwrap();
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error");
    assert_eq!(e["error"]["code"], "invalid_event");
    assert!(e["event_id"].as_str().unwrap().starts_with("event_"));

    send(
        &mut ws,
        json!({"type": "response.audio.delta", "event_id": "c1"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "invalid_event");
    assert_eq!(e["error"]["event_id"], "c1");
    assert_eq!(e["error"]["param"], "type");

    // A known event of the wrong shape.
    send(
        &mut ws,
        json!({"type": "conversation.item.delete", "event_id": "c2"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "invalid_event");
    assert_eq!(e["error"]["event_id"], "c2");

    // A session.update the cascade cannot serve changes nothing.
    send(
        &mut ws,
        json!({"type": "session.update", "event_id": "c3",
               "session": {"type": "realtime", "lmgw": {"nonsense": 1}}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["param"], "session.lmgw");
    assert_eq!(e["error"]["event_id"], "c3");

    // A speed outside the GA range, 0.25 to 1.5 (package B review 10) — the
    // session's, and a response's own.
    send(
        &mut ws,
        json!({"type": "session.update", "event_id": "c4",
               "session": {"type": "realtime", "audio": {"output": {"speed": 2.0}}}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["type"], "invalid_request_error");
    assert_eq!(e["error"]["code"], "invalid_value");
    assert_eq!(e["error"]["param"], "session.audio.output.speed");
    assert_eq!(e["error"]["event_id"], "c4");
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "c5",
               "response": {"audio": {"output": {"speed": 0.1}}}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "invalid_value");
    assert_eq!(e["error"]["param"], "response.audio.output.speed");
    assert_eq!(e["error"]["event_id"], "c5");

    // An audio response with no TTS alias configured: said so before it
    // starts — echoing the client's id.
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "agents_js_response_create_1"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "tts_not_configured");
    assert_eq!(e["error"]["event_id"], "agents_js_response_create_1");

    // Still open, still answering.
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime", "instructions": "ok"}}),
    )
    .await;
    let u = next_event(&mut ws).await;
    assert_eq!(u["type"], "session.updated");
    assert_eq!(u["session"]["instructions"], "ok");
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_s_scope_is_checked_before_the_upgrade_and_on_update() {
    let (_s, addr) = gateway(
        true,
        Some(KeyPolicy {
            scope_mode: ScopeMode::Allow,
            scope_patterns: "chatty".into(),
            ..Default::default()
        }),
        |_| {},
    )
    .await;
    let bearer = format!("Bearer {KEY}");
    let auth = [("authorization", bearer.as_str())];

    let (status, body) = refused(&addr, "/v1/realtime?model=other", &auth).await;
    assert_eq!(status, 403);
    assert_eq!(code(&body), "key_scope");

    let mut ws = open(&addr, "/v1/realtime?model=chatty", &auth).await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update", "event_id": "u1",
               "session": {"type": "realtime", "model": "other", "instructions": "x"}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error");
    assert_eq!(e["error"]["code"], "key_scope");
    assert_eq!(e["error"]["param"], "session.model");
    assert_eq!(e["error"]["event_id"], "u1");

    // The refused update changed nothing.
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime"}}),
    )
    .await;
    let u = next_event(&mut ws).await;
    assert_eq!(u["session"]["model"], "chatty");
    assert_eq!(u["session"]["lmgw"]["resolved"]["chat"], "chatty");
    assert_ne!(u["session"]["instructions"], "x");

    // A word-check alias of the client's own is checked like a model (W1).
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "lmgw": {"barge_in_check_alias": "other"}}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "key_scope", "{e}");
    assert_eq!(e["error"]["param"], "session.lmgw.barge_in_check_alias");
}

#[tokio::test]
async fn an_open_session_holds_one_concurrency_slot() {
    let (state, addr) = gateway(
        true,
        Some(KeyPolicy {
            concurrency_limit: 1,
            ..Default::default()
        }),
        |_| {},
    )
    .await;
    let bearer = format!("Bearer {KEY}");
    let models = || async {
        reqwest::Client::new()
            .get(format!("http://{addr}/v1/models"))
            .header("authorization", &bearer)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(models().await, 200, "the slot is free to begin with");

    let mut ws = open(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", bearer.as_str())],
    )
    .await;
    next_event(&mut ws).await;
    // The 101 is long gone; the session still counts as the one request.
    assert_eq!(models().await, 429);
    // A second session is refused by the gate as well; its row is labelled
    // like the session's own (§11), the models request's like `/v1`'s.
    let (status, _) = refused(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(status, 429);
    let labels: Vec<(String,)> =
        sqlx::query_as("SELECT ingress_proto FROM request_logs WHERE status = 429 ORDER BY id")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(labels, [("openai".to_string(),), ("realtime".to_string(),)]);

    ws.close(None).await.unwrap();
    drop(ws);
    // Released when the session task ends, which follows the close.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = models().await;
        if status == 200 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slot was not released after the socket closed (last status {status})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[test]
fn a_frame_is_always_bounded() {
    use lmgw_core::config::RealtimeSettings;
    use lmgw_core::realtime::Limits;
    let mb = 1024 * 1024;
    let limits = |message, frame| {
        Limits::from_settings(&RealtimeSettings {
            max_message_mb: message,
            max_frame_mb: frame,
            ..Default::default()
        })
    };
    // No frame bound of its own: the message bounds it — tungstenite
    // reserves a frame's declared length before reading it.
    let l = limits(8, 0);
    assert_eq!((l.max_message, l.max_frame), (8 * mb, 8 * mb));
    // A frame setting larger than the message is the message's.
    assert_eq!(limits(8, 32).max_frame, 8 * mb);
    // An unbounded message keeps its own frame bound.
    let l = limits(0, 4);
    assert_eq!((l.max_message, l.max_frame), (usize::MAX, 4 * mb));
    // Nothing left to bound a frame with: the defaults.
    let d = RealtimeSettings::default();
    let l = limits(0, 0);
    assert_eq!(
        (l.max_message, l.max_frame),
        (d.max_message_mb as usize * mb, d.max_frame_mb as usize * mb)
    );
}

#[tokio::test]
async fn stored_limits_both_unbounded_fall_back_to_the_defaults() {
    let (state, _addr) = gateway(false, None, |s| {
        s.realtime.max_message_mb = 0;
        s.realtime.max_frame_mb = 0;
    })
    .await;
    let d = lmgw_core::config::RealtimeSettings::default();
    let rt = &state.snapshot().settings.realtime;
    assert_eq!(
        (rt.max_message_mb, rt.max_frame_mb),
        (d.max_message_mb, d.max_frame_mb)
    );
}

#[tokio::test]
async fn a_frame_bounded_by_the_message_names_the_message_setting() {
    let (_s, addr) = gateway(false, None, |s| {
        s.realtime.max_message_mb = 1;
        s.realtime.max_frame_mb = 0;
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;
    let _ = ws.send(Message::text("x".repeat(2 * 1024 * 1024))).await;
    let close = loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Close(frame)))) => break frame,
            Ok(Some(Ok(_))) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    };
    let frame = close.expect("the close carries a reason");
    assert_eq!(frame.code, CloseCode::Size);
    let reason = frame.reason.as_str();
    assert!(
        reason.contains("realtime.max_message_mb (1 MiB)"),
        "{reason}"
    );
    assert!(!reason.contains("max_frame_mb"), "{reason}");
}

#[tokio::test]
async fn an_oversized_message_closes_with_a_reason_naming_the_setting() {
    let (_s, addr) = gateway(false, None, |s| {
        s.realtime.max_message_mb = 1;
        s.realtime.max_frame_mb = 1;
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;

    let big = "x".repeat(2 * 1024 * 1024);
    // The send may itself fail once the server has closed; the close frame
    // is what is being asserted.
    let _ = ws.send(Message::text(big)).await;
    let close = loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Close(frame)))) => break frame,
            Ok(Some(Ok(_))) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    };
    let frame = close.expect("the close carries a reason");
    assert_eq!(frame.code, CloseCode::Size);
    assert!(
        frame.reason.as_str().contains("realtime.max_message_mb"),
        "{}",
        frame.reason
    );
}
