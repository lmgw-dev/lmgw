//! The agent proxy behind a trusted proxy (headless server design §4.5,
//! D33, D49): the app is told the client and the scheme the proxy
//! forwarded, and a client the proxy named no address for goes without
//! `X-Forwarded-For`, never with the proxy's own.

use std::sync::Arc;

use lmgw_core::agents::service;
use serde_json::{json, Value};

use super::Gw;
use crate::agents_service::{doc, install, Fake};

#[tokio::test]
async fn an_app_is_told_the_forwarded_client_and_scheme() {
    let prefix = format!("lmgwf{}", std::process::id());
    let gw = Gw::start(|s| {
        s.trusted_proxies = vec!["127.0.0.1/32".into()];
        s.container_prefix = prefix;
    })
    .await;
    gw.state
        .set_agent_spawner_for_tests(Arc::new(Fake::default()));
    let plain = crate::common::Gw {
        base: gw.base.clone(),
        key: gw.key(),
    };
    install(&plain, &doc("localhost/board:1", 0, 10, "")).await;
    let addr: std::net::SocketAddr = gw.base.trim_start_matches("http://").parse().unwrap();
    let client = reqwest::Client::builder()
        .resolve("board.localhost", addr)
        .build()
        .unwrap();
    let seen = |headers: Vec<(&'static str, &'static str)>| {
        let client = client.clone();
        async move {
            let r = headers.iter().fold(
                client.get(format!("http://board.localhost:{}/seen", addr.port())),
                |rb, (k, v)| rb.header(*k, *v),
            );
            let r = r.send().await.unwrap();
            assert_eq!(r.status().as_u16(), 200);
            r.json::<Value>().await.unwrap()
        }
    };
    // The client's own hop on the left is not read; the proxy appended
    // the one it saw.
    let s = seen(vec![
        ("x-forwarded-for", "198.51.100.1, 203.0.113.5"),
        ("x-forwarded-proto", "https"),
    ])
    .await;
    assert_eq!(s["x-forwarded-for"], json!("203.0.113.5"), "{s}");
    assert_eq!(s["x-forwarded-proto"], json!("https"), "{s}");
    // The agent's origin, built from `bind_addr` as without a proxy.
    assert!(
        s["x-forwarded-host"]
            .as_str()
            .unwrap()
            .starts_with("board.localhost:"),
        "{s}"
    );
    // The proxy named no client (only its scheme, or `unknown`): no
    // X-Forwarded-For at all, never the proxy's 127.0.0.1.
    for headers in [
        vec![("x-forwarded-proto", "https")],
        vec![
            ("x-forwarded-for", "unknown"),
            ("x-forwarded-proto", "http"),
        ],
    ] {
        let s = seen(headers.clone()).await;
        assert!(s.get("x-forwarded-for").is_none(), "{headers:?}: {s}");
        assert!(s.get("x-forwarded-proto").is_some(), "{s}");
    }
    // A direct request from the same address: its own peer and scheme.
    let s = seen(Vec::new()).await;
    assert_eq!(s["x-forwarded-for"], json!("127.0.0.1"), "{s}");
    assert_eq!(s["x-forwarded-proto"], json!("http"), "{s}");
    service::stop(&gw.state, "board", "test over").await;
    gw.stop().await;
}
