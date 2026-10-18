//! What the credential ops, the dashboard's settings save and the update check
//! answer over HTTP (`POST /api/op/<name>`): each answer reads back through
//! the type the document describes, and keeps the keys it has always sent.
//! `key_create` answers one of two types by the kind of row,
//! so the document's `anyOf` is checked too.

use lmgw_api_types as dto;
use lmgw_core::state::AppState;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, Gw};

async fn op(gw: &Gw, name: &str, body: Value) -> Value {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let live: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{name}: {live}");
    live
}

/// The keys of an answer, sorted: the set an action sends and no other.
fn keys(live: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = live
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    k.sort_unstable();
    k
}

async fn gateway() -> Gw {
    common::serve(AppState::init_for_tests().await.unwrap()).await
}

/// A client key and an owner key are born with their key in the clear, in
/// `key`; a scope given at creation changes nothing about the answer.
#[tokio::test]
async fn a_created_client_or_owner_key_is_the_documented_type() {
    let gw = gateway().await;

    let live = op(&gw, "key_create", json!({"name": "ci"})).await;
    let made = common::round_trips::<dto::KeyCreated>("client key", &live);
    assert_eq!(keys(&live), ["id", "key", "message", "name", "ok"]);
    assert_eq!(made.name, "ci");
    assert!(made.ok && made.id > 0);
    assert!(made.key.starts_with("lmgw-"), "{live}");
    assert_eq!(made.key.len(), "lmgw-".len() + 32, "{live}");
    assert_eq!(
        made.message,
        "key 'ci' created — copy it now, it is not shown again"
    );

    let live = op(
        &gw,
        "key_create",
        json!({"name": "scoped", "kind": "client", "scope_mode": "allow",
               "scope_patterns": "a*", "tool_scope_mode": "deny",
               "tool_scope_patterns": "docs__*"}),
    )
    .await;
    let scoped = common::round_trips::<dto::KeyCreated>("scoped client key", &live);
    assert_eq!(keys(&live), ["id", "key", "message", "name", "ok"]);
    assert_ne!(scoped.id, made.id);

    // An owner key names itself: the prefix is the server's.
    let live = op(&gw, "key_create", json!({"name": "boss", "kind": "owner"})).await;
    let owner = common::round_trips::<dto::KeyCreated>("owner key", &live);
    assert_eq!(keys(&live), ["id", "key", "message", "name", "ok"]);
    assert_eq!(owner.name, "owner:boss");
    assert!(owner.key.starts_with("lmgw-owner-"), "{live}");
    assert!(owner.message.starts_with("owner key 'owner:boss' created"));
}

/// An owner key is handed out as `key`, by a reveal (the stored one) and a
/// rotation (the new one); a delete is the bare `{ok, message}`.
#[tokio::test]
async fn an_owner_key_handed_out_is_the_documented_type() {
    let gw = gateway().await;
    let live = op(&gw, "key_create", json!({"name": "boss", "kind": "owner"})).await;
    let made = common::round_trips::<dto::KeyCreated>("owner key", &live);

    let live = op(&gw, "key_reveal", json!({"id": made.id})).await;
    let shown = common::round_trips::<dto::KeySecret>("key_reveal", &live);
    assert_eq!(keys(&live), ["id", "key", "message", "name", "ok"]);
    assert_eq!((shown.id, shown.name.as_str()), (made.id, "owner:boss"));
    assert_eq!(shown.key, made.key);
    assert_eq!(shown.message, "'owner:boss' copied");

    let live = op(&gw, "key_rotate", json!({"id": made.id})).await;
    let rotated = common::round_trips::<dto::KeySecret>("key_rotate", &live);
    assert_eq!(keys(&live), ["id", "key", "message", "name", "ok"]);
    assert_eq!(rotated.id, made.id);
    assert_ne!(rotated.key, made.key);
    assert!(rotated.key.starts_with("lmgw-owner-"), "{live}");
    assert_eq!(
        rotated.message,
        "'owner:boss' is rotated — the old value no longer works"
    );
    // What a reveal shows now is the rotated value.
    let live = op(&gw, "key_reveal", json!({"id": made.id})).await;
    assert_eq!(
        common::round_trips::<dto::KeySecret>("key_reveal", &live).key,
        rotated.key
    );

    let live = op(&gw, "key_delete", json!({"id": made.id})).await;
    let gone = common::round_trips::<dto::MessageAck>("key_delete", &live);
    assert_eq!(live, json!({"ok": true, "message": "key revoked"}));
    assert!(gone.ok);
}

/// A device is paired with a pairing link v2, and given a new token, with one
/// answer: the link that carries the token, the URLs it names, whether it is
/// this computer, when the token lapses and the CA's `fp`. Its device ops
/// answer their own types; `key_rotate` refuses a device (device link design
/// §9.4).
#[tokio::test]
async fn a_paired_device_and_its_new_token_are_the_documented_type() {
    let d = crate::support::device_link::gateway().await;
    let gw = &d.gw;
    let all = [
        "fp",
        "id",
        "link",
        "local_only",
        "message",
        "name",
        "ok",
        "token_expires_at",
        "urls",
    ];

    // This computer: the device listener's loopback URL alone.
    let live = op(
        gw,
        "key_create",
        json!({"kind": "device", "name": "desktop", "local_only": true}),
    )
    .await;
    let paired = common::round_trips::<dto::DevicePaired>("device pair", &live);
    assert_eq!(keys(&live), all);
    assert_eq!(paired.name, "device:desktop");
    assert_eq!(paired.urls, [format!("https://127.0.0.1:{}", d.port)]);
    assert!(paired.local_only);
    assert_eq!(paired.fp, d.ca.fp());
    assert!(
        paired.link.starts_with("lmgw-pair:?v=2&name=desktop&url="),
        "{live}"
    );
    let link = lmgw_client::pairing::PairingLink::parse(&paired.link).unwrap();
    assert_eq!(link.fp.to_string(), paired.fp);
    assert!(
        paired
            .message
            .starts_with("device 'desktop' is waiting for its device"),
        "{live}"
    );

    // Another device at the URLs the owner gave: no loopback, and the
    // loopback-only listener is said not to reach it.
    let live = op(
        gw,
        "key_create",
        json!({"kind": "device", "name": "phone",
               "urls": ["https://192.0.2.1:8788/", "https://lmgw.example.net"]}),
    )
    .await;
    let phone = common::round_trips::<dto::DevicePaired>("device pair, urls", &live);
    assert_eq!(keys(&live), all);
    assert_eq!(
        phone.urls,
        ["https://192.0.2.1:8788", "https://lmgw.example.net"]
    );
    assert!(!phone.local_only);
    assert!(phone.message.contains("will not reach it"), "{live}");

    // A new token keeps the row and mints a new link.
    let live = op(gw, "device_token", json!({"id": paired.id})).await;
    let again = common::round_trips::<dto::DevicePaired>("device token", &live);
    assert_eq!(keys(&live), all);
    assert_eq!(
        (again.id, again.name.as_str()),
        (paired.id, "device:desktop")
    );
    assert_ne!(again.link, paired.link);
    assert_eq!(again.urls, paired.urls);
    assert!(again
        .message
        .starts_with("a new pairing link for device 'desktop'"));

    let live = op(gw, "device_token_end", json!({"id": paired.id})).await;
    let ended = common::round_trips::<dto::DeviceTokenEnded>("device token end", &live);
    assert_eq!(keys(&live), ["ended", "id", "message", "name", "ok"]);
    assert!(ended.ended);

    // A device holds no secret of lmgw's making: no rotate, no reveal.
    for name in ["key_rotate", "key_reveal"] {
        let resp = gw
            .client()
            .post(format!("{gw}/api/op/{name}"))
            .json(&json!({"id": phone.id}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{name}");
        let v: Value = resp.json().await.unwrap();
        assert!(
            v["message"].as_str().unwrap().contains("new pairing token"),
            "{v}"
        );
    }
}

/// `key_create` answers one of two types; every key and device op's type is
/// a component a client generator can name.
#[tokio::test]
async fn the_document_describes_the_answers_of_the_key_and_device_ops() {
    let doc = lmgw_core::openapi::admin_doc();
    let answer = |op: &str| -> Value {
        doc["paths"][format!("/api/op/{op}")]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"]
            .clone()
    };
    let arms: Vec<String> = answer("key_create")["anyOf"]
        .as_array()
        .unwrap_or_else(|| panic!("key_create: an anyOf of the two answers"))
        .iter()
        .filter_map(|a| a["$ref"].as_str().map(String::from))
        .collect();
    assert_eq!(
        arms,
        [
            "#/components/schemas/KeyCreated",
            "#/components/schemas/DevicePaired"
        ]
    );
    for (op, ty) in [
        ("key_rotate", "KeySecret"),
        ("device_token", "DevicePaired"),
        ("device_token_end", "DeviceTokenEnded"),
        ("device_confirm", "DeviceConfirmed"),
        ("device_ca_rotate", "DeviceCaRotated"),
    ] {
        // An op's one answer type is its schema, inline, under its name.
        let schema = answer(op);
        assert_eq!(schema["title"], ty, "{op}: {schema}");
    }
    // What every route sends is required.
    let required = |name: &str| -> Vec<String> {
        let mut r: Vec<String> = doc["components"]["schemas"][name]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} has no required list"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        r.sort();
        r
    };
    assert_eq!(
        required("DevicePaired"),
        [
            "fp",
            "id",
            "link",
            "local_only",
            "message",
            "name",
            "ok",
            "token_expires_at",
            "urls"
        ]
    );
    let mut secret: Vec<String> = answer("key_rotate")["required"]
        .as_array()
        .unwrap_or_else(|| panic!("key_rotate's answer has no required list"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    secret.sort();
    assert_eq!(secret, ["id", "key", "message", "name", "ok"]);
}

/// The dashboard's settings save answers a sentence, with the notes the save
/// has to make behind it.
#[tokio::test]
async fn the_settings_save_answers_a_message() {
    let gw = gateway().await;

    let live = op(&gw, "settings_set_full", json!({"currency": "EUR"})).await;
    let saved = common::round_trips::<dto::MessageAck>("settings_set_full", &live);
    assert_eq!(live, json!({"ok": true, "message": "settings saved"}));
    assert!(saved.ok);

    let live = op(
        &gw,
        "settings_set_full",
        json!({"router": {"image": "example.test/llama:1"}}),
    )
    .await;
    let noted = common::round_trips::<dto::MessageAck>("settings_set_full, note", &live);
    assert_eq!(
        noted.message,
        "settings saved — chat class definition changed — running chat models keep the \
         previous one until they restart"
    );
}

/// The update check answers a sentence whether or not a newer release is out.
#[tokio::test]
async fn the_update_check_answers_a_message() {
    let _env = common::process_env_lock().await;
    let gw = gateway().await;
    let server = MockServer::start().await;
    let manifest = |version: &str| {
        json!({"version": version, "notes": "", "pub_date": "",
               "rpm": {"file": "lmgw.rpm", "url": format!("{}/lmgw.rpm", server.uri())}})
    };
    std::env::set_var(
        "LMGW_UPDATE_ENDPOINT",
        format!("{}/latest.json", server.uri()),
    );

    Mock::given(method("GET"))
        .and(path("/latest.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(manifest("999.0.0")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let live = op(&gw, "update_check", json!({})).await;
    let newer = common::round_trips::<dto::MessageAck>("update_check, newer", &live);
    assert_eq!(
        newer.message,
        format!(
            "update available: 999.0.0 (you have {}) — install it from the tray menu",
            env!("CARGO_PKG_VERSION")
        )
    );

    Mock::given(method("GET"))
        .and(path("/latest.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(manifest(env!("CARGO_PKG_VERSION"))))
        .mount(&server)
        .await;
    let live = op(&gw, "update_check", json!({})).await;
    let same = common::round_trips::<dto::MessageAck>("update_check, current", &live);
    assert_eq!(
        same.message,
        format!("up to date ({})", env!("CARGO_PKG_VERSION"))
    );
}
