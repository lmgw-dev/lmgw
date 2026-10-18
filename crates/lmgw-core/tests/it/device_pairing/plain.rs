//! No bearer opens a device (device link design D12, §4.8): a
//! `lmgw-device-` bearer on the plain listener is 401 `device_key_unknown`
//! from loopback too, and a row paired before TLS is named; owner and client
//! keys and the dashboard's cookie are as they were.

use serde_json::{json, Value};

use super::world;

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

#[tokio::test]
async fn a_device_bearer_opens_nothing_on_the_plain_listener() {
    let w = world().await;
    let old = "lmgw-device-0123456789abcdef";
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, kind) VALUES ('device:desk', ?1, 1, 'device')",
    )
    .bind(format!("v1:{}", lmgw_core::config::hash_api_key(old)))
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let v1 = "device 'desk' was paired with a link from before TLS, which this lmgw no longer \
              accepts — pair it again on Usage → Keys → Devices";

    let resp = bearer(old)
        .get(format!("{}/chat/api/threads", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        (&v["code"], &v["message"]),
        (&json!("device_key_unknown"), &json!(v1))
    );
    let resp = bearer(old)
        .get(format!("{}/v1/models", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "device_key_unknown", "{v}");
    assert_eq!(v["error"]["message"], v1, "{v}");
    // A device's bearer with no row behind it: the plain words.
    let resp = bearer("lmgw-device-ffff")
        .get(format!("{}/chat/api/threads", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["code"], "device_key_unknown", "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("pair the device again"),
        "{v}"
    );

    // The owner's cookie and bearer, and a client key, as they were.
    let resp =
        w.gw.client()
            .get(format!("{}/chat/api/threads", w.gw))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    let (s, v) = w.op("key_create", json!({ "name": "laptop" })).await;
    assert_eq!(s, 200, "{v}");
    let resp = bearer(v["key"].as_str().unwrap())
        .get(format!("{}/v1/models", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
