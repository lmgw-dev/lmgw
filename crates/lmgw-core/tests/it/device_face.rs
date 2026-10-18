//! The route walk over both faces (device link design §4.6, WP2): every
//! `CAPABILITY_TABLE` row asked over the device listener with a confirmed
//! device's certificate. An `Inference` or `Chat` row, and the device
//! listener's own (`DEVICE_ROUTES`), reaches its handler (whatever the
//! handler then says of the probe); every other row is the 404 an unknown
//! path is, with nothing in its body. On the plain listener, the device
//! listener's own routes are that 404, a body past their limit included.

use std::time::Duration;

use lmgw_core::principal::Cap;
use lmgw_core::server::{serves_on, Via, CAPABILITY_TABLE, DEVICE_ROUTES};
use serde_json::json;

use crate::support::device_link::gateway;

/// The capture every `{..}` is asked with: an id nothing has.
fn fill(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if !seg.starts_with('{') {
                seg.to_string()
            } else if seg.contains("job") {
                "1".into()
            } else {
                "face-walk-probe".into()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[tokio::test]
async fn every_row_is_on_the_device_face_exactly_when_it_is_inference_or_chat() {
    let gw = gateway().await;
    let device = gw.device("walker", true).await;
    let https = reqwest::Client::builder()
        .tls_backend_preconfigured(crate::support::device_link::client_config(
            gw.ca.certificate(),
            Some(&device.identity),
            &[&rustls::version::TLS13],
        ))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let mut on_face = 0;
    for (method, path, cap) in CAPABILITY_TABLE {
        let verb = if *method == "*" { "GET" } else { method };
        let mut req = https
            .request(verb.parse().unwrap(), gw.url(&fill(path)))
            .header("accept", "application/json");
        if verb == "POST" {
            req = req.json(&json!({}));
        }
        let resp = req
            .send()
            .await
            .unwrap_or_else(|e| panic!("{verb} {path}: {e}"));
        let status = resp.status().as_u16();
        let own = DEVICE_ROUTES.contains(&(*method, *path));
        let wanted_on_face = own || matches!(cap, Cap::Inference | Cap::Chat);
        assert_eq!(
            serves_on(Via::Devices, &verb.parse().unwrap(), path, *cap),
            wanted_on_face,
            "{verb} {path}: the face rule"
        );
        // Only a 404's body is read: an answered stream (a feed, `/mcp`'s
        // GET) would hold the walk for as long as it lasts.
        let face_404 = status == 404 && resp.bytes().await.unwrap_or_default().is_empty();
        if wanted_on_face {
            on_face += 1;
            assert!(
                !face_404,
                "{verb} {path} ({cap:?}): off the face, wanted on it"
            );
            // The certificate's device holds Inference and Chat; the host
            // link's own handler wants a hosting grant this device lacks.
            if *path != "/mcp/host" {
                assert!(
                    status != 401 && status != 403,
                    "{verb} {path} ({cap:?}): refused with {status}"
                );
            }
        } else {
            assert!(
                face_404,
                "{verb} {path} ({cap:?}): {status}, wanted the face's 404"
            );
        }
    }
    assert!(on_face > 50, "only {on_face} rows were on the device face");
}

/// The plain listener answers the device listener's own routes as it
/// answers an unknown path, before their 16 KiB limit is read: the gate's
/// face check sits outside the routes' body limit (WP2's note).
#[tokio::test]
async fn the_device_routes_are_a_404_on_the_plain_listener() {
    let gw = gateway().await;
    let past_the_limit = "x".repeat(lmgw_api_types::device_link::BODY_LIMIT_BYTES + 1);
    for (method, path) in DEVICE_ROUTES {
        assert!(!serves_on(
            Via::Plain,
            &method.parse().unwrap(),
            path,
            Cap::Public
        ));
        for body in [json!({}).to_string(), past_the_limit.clone()] {
            let r = gw
                .gw
                .client()
                .request(method.parse().unwrap(), format!("{}{path}", gw.gw))
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "{method} {path}");
        }
    }
}
