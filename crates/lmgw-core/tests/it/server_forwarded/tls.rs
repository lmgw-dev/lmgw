//! Proxy mode on the TLS listener (headless server design §4.5, D51): the
//! edge stands in front of it too, so a TLS-to-TLS proxy's forwarded
//! client, scheme and host reach the cookie and the same-origin rule, and
//! its refusals and first peer are the TLS listener's.

use serde_json::json;

use crate::main_tls::{serve, Pki};

#[tokio::test]
async fn a_trusted_peer_on_the_tls_listener_has_its_headers_read() {
    let pki = Pki::new();
    let gw = serve(&pki, |s| s.trusted_proxies = vec!["127.0.0.1/32".into()]).await;
    let key = crate::common::dashboard_key(&gw.state);
    let base = format!("https://localhost:{}", gw.tls.port());
    let http = pki.http_resolving(Some(("localhost", gw.tls)));
    // The browser's origin is the proxy's host, not the listener's.
    let r = http
        .post(format!("{base}/api/session"))
        .header("x-forwarded-for", "203.0.113.5")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-host", "lmgw.example")
        .header("origin", "https://lmgw.example")
        .json(&json!({ "token": key }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);
    let cookie = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(cookie.starts_with("__Host-lmgw_session="), "{cookie}");
    // Without the forwarded host the same origin is another one.
    let r = http
        .post(format!("{base}/api/session"))
        .header("origin", "https://lmgw.example")
        .json(&json!({ "token": key }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    // Both families from the trusted peer: refused on this listener too.
    let r = http
        .get(format!("{base}/api/status"))
        .bearer_auth(&key)
        .header("x-forwarded-for", "203.0.113.5")
        .header("forwarded", "for=203.0.113.6")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(
        body["code"],
        json!(lmgw_core::server::forwarded::REFUSED_CODE),
        "{body}"
    );
    let card =
        lmgw_core::server::forwarded::status(&gw.state.snapshot().settings, &gw.state.proxy_seen);
    assert!(
        card.first_peers
            .iter()
            .any(|l| l.contains("on the tls listener") && l.contains("a trusted proxy")),
        "{card:?}"
    );
    assert!(
        card.refused
            .as_deref()
            .is_some_and(|r| r.contains("on the tls listener")),
        "{card:?}"
    );
    gw.stop().await;
}
