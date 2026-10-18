//! The `devices` settings class (§9.1) over the real router: read on the
//! settings page, saved by both save paths with each bound refused by name
//! and the answer saying when a change applies, clamped on load, and no
//! self-admin tool's to change (an access setting).

use lmgw_api_types::device_link::DevicesSettings as Wire;
use lmgw_api_types::SettingsFull;
use lmgw_core::config::{DevicesListen, DevicesSettings, DEVICES_BOUNDS};
use lmgw_core::state::AppState;
use serde_json::{json, Value};

use crate::common::{self, Gw};

async fn post(gw: &Gw, path: &str, body: Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn full(gw: &Gw) -> (SettingsFull, Value) {
    let live: Value = gw
        .client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (common::round_trips("settings-full", &live), live)
}

/// The keys of a value's JSON object.
fn keys(v: Value) -> Vec<String> {
    let Value::Object(m) = v else {
        panic!("not an object")
    };
    m.keys().cloned().collect()
}

#[tokio::test]
async fn the_settings_page_reads_the_devices_class() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    let (full, live) = full(&gw).await;
    let d = DevicesSettings::default();
    let port = state
        .snapshot()
        .settings
        .bind_addr
        .parse::<std::net::SocketAddr>()
        .unwrap()
        .port();
    assert_eq!(
        full.devices,
        Wire {
            listener: false,
            listen: "loopback".into(),
            port: None,
            port_effective: Some(port + 1),
            token_minutes: 10,
            client_cert_days: 365,
            server_cert_days: 30,
            handshake_timeout_s: 10,
            header_timeout_s: 30,
            unauthenticated_timeout_s: 10,
            enrol_refuse_per_min: 120,
            unauthenticated_max: 64,
            unauthenticated_per_peer: 8,
        }
    );
    assert_eq!(full.devices.token_minutes, d.token_minutes);
    assert!(live["devices"]["port"].is_null());

    // The wire mirror is the config class plus the derived port.
    let mut config = keys(serde_json::to_value(&d).unwrap());
    config.push("port_effective".into());
    config.sort();
    let mut wire = keys(serde_json::to_value(Wire::default()).unwrap());
    wire.sort();
    assert_eq!(config, wire);
    // Every bounded key has its stated range and a default within it.
    for b in &DEVICES_BOUNDS {
        let v = serde_json::to_value(&d).unwrap()[b.key].as_u64().unwrap() as u32;
        assert!((b.bound.min..=b.bound.max).contains(&v), "{}", b.key);
    }
}

#[tokio::test]
async fn both_saves_take_the_bounds_and_say_when_they_apply() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;

    let (status, res) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "devices": { "token_minutes": 30, "header_timeout_s": 60 } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let msg = res["message"].as_str().unwrap();
    assert!(
        msg.contains("devices.token_minutes applies to the next pairing token"),
        "{msg}"
    );
    assert!(
        msg.contains("devices.header_timeout_s applies to the next connection"),
        "{msg}"
    );
    let d = state.snapshot().settings.devices.clone();
    assert_eq!((d.token_minutes, d.header_timeout_s), (30, 60));

    let (status, res) = post(
        &gw,
        "/api/op/settings_set",
        json!({ "devices": { "enrol_refuse_per_min": 500 } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["changed"], json!(["devices.enrol_refuse_per_min"]));
    assert!(
        res["message"]
            .as_str()
            .unwrap()
            .contains("(devices.enrol_refuse_per_min applies at once)"),
        "{res}"
    );
    assert_eq!(state.snapshot().settings.devices.enrol_refuse_per_min, 500);
    let (full, _) = full(&gw).await;
    assert_eq!(full.devices.enrol_refuse_per_min, 500);

    // Out of range: refused by name with the range, nothing saved, on
    // either path.
    for path in ["/api/op/settings_set_full", "/api/op/settings_set"] {
        for (body, key) in [
            (
                json!({ "token_minutes": 0 }),
                "devices.token_minutes must be from 1 to 1440",
            ),
            (
                json!({ "client_cert_days": 3651 }),
                "devices.client_cert_days must be from 1 to 3650",
            ),
            (
                json!({ "token_minutes": 5, "handshake_timeout_s": -1 }),
                "devices.handshake_timeout_s",
            ),
        ] {
            let (status, res) = post(&gw, path, json!({ "devices": body })).await;
            assert_eq!(status, 400, "{path} {body}: {res}");
            assert!(res.to_string().contains(key), "{path}: {res}");
        }
        // The listener's scope and port, out of range, refused by name
        // (WP2 takes them; `device_listener.rs` has their effects).
        for (body, key) in [
            (json!({ "listen": "the moon" }), "devices.listen must be"),
            (json!({ "port": 0 }), "devices.port must be from 1 to 65535"),
        ] {
            let (status, res) = post(&gw, path, json!({ "devices": body })).await;
            assert_eq!(status, 400, "{path} {body}: {res}");
            assert!(res.to_string().contains(key), "{path}: {res}");
        }
    }
    let d = state.snapshot().settings.devices.clone();
    assert_eq!(
        (
            d.token_minutes,
            d.handshake_timeout_s,
            d.client_cert_days,
            d.listener
        ),
        (30, 10, 365, false)
    );
}

#[tokio::test]
async fn stored_values_out_of_range_are_clamped_on_load() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES ('settings', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(
        r#"{"devices":{"token_minutes":0,"server_cert_days":99999,"listen":"the moon",
            "port":8900,"listener":true}}"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    let s = lmgw_core::store::load_settings(&pool).await.unwrap();
    assert_eq!(s.devices.token_minutes, 1);
    assert_eq!(s.devices.server_cert_days, 3650);
    // An odd scope reads as the narrowest one.
    assert_eq!(s.devices.listen, DevicesListen::Loopback);
    assert_eq!((s.devices.port, s.devices.listener), (Some(8900), true));
    assert_eq!(
        s.devices.client_cert_days, 365,
        "absent keys take their default"
    );

    // Port 0 is no port: the default (the HTTP port + 1) stands in.
    sqlx::query("UPDATE settings SET value = ?1 WHERE key = 'settings'")
        .bind(r#"{"devices":{"port":0}}"#)
        .execute(&pool)
        .await
        .unwrap();
    let s = lmgw_core::store::load_settings(&pool).await.unwrap();
    assert_eq!(s.devices.port, None);
    assert_eq!(s.devices.effective_port("127.0.0.1:8787"), Ok(8788));

    // The three scopes round-trip as their text.
    for (text, want) in [
        ("loopback", DevicesListen::Loopback),
        ("all", DevicesListen::All),
        (
            "198.51.100.20",
            DevicesListen::Addr("198.51.100.20".parse().unwrap()),
        ),
        ("::1", DevicesListen::Addr("::1".parse().unwrap())),
    ] {
        let v: DevicesListen = serde_json::from_value(json!(text)).unwrap();
        assert_eq!(v, want);
        assert_eq!(serde_json::to_value(v).unwrap(), json!(text));
    }
}
