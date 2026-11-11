//! The agent proxy on the TLS listener (headless server design §4.3, D33):
//! an agent origin under a suffix the certificate covers is served there,
//! and the app is told `https` in `X-Forwarded-Proto`, with the authority
//! the browser used in `X-Forwarded-Host`; a start that serves no TLS
//! moves the origin back and stops the app container holding the old one.

use std::sync::Arc;

use lmgw_core::agents::service;
use serde_json::{json, Value};

use super::Pki;
use crate::agents_service::{doc, install, Fake};
use crate::common::Gw;

#[tokio::test]
async fn on_the_tls_listener_an_app_is_told_https() {
    let pki = Pki::new();
    pki.write(&["localhost", "127.0.0.1", "*.lmgw-apps.example"], 1);
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    state.set_agent_spawner_for_tests(Arc::new(Fake::default()));
    let prefix = format!("lmgwt{}", std::process::id());
    let gw = super::serve_on(state, &pki, |s| {
        s.container_prefix = prefix;
        s.agent_origin_suffix = "lmgw-apps.example".into();
    })
    .await;
    let plain = Gw {
        base: format!("http://{}", gw.plain),
        key: crate::common::dashboard_key(&gw.state),
    };
    install(&plain, &doc("localhost/board:1", 0, 10, "")).await;
    let port = gw.tls.port();
    let seen: Value = pki
        .http_resolving(Some(("board.lmgw-apps.example", gw.tls)))
        .get(format!("https://board.lmgw-apps.example:{port}/seen"))
        .header("x-forwarded-proto", "http")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(seen["x-forwarded-proto"], json!("https"), "{seen}");
    assert_eq!(
        seen["x-forwarded-host"],
        json!(format!("board.lmgw-apps.example:{port}")),
        "{seen}"
    );
    assert!(
        gw.state.agent_services.get("board").is_some(),
        "started on demand"
    );

    // The next start whose pair does not load serves no TLS: the origin
    // moves back to the plain listener's, and the app container holding
    // the https one is stopped at that start (D45).
    let state = gw.stop().await;
    std::fs::write(pki.cert_path(), "not a certificate\n").unwrap();
    let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped.await;
        },
        None,
    ));
    crate::common::patience::until("the app container holding https is stopped", || {
        state.agent_services.get("board").is_none()
    })
    .await;
    assert_eq!(state.snapshot().serving_tls, None);
    service::stop(&state, "board", "test over").await;
    let _ = stop.send(());
    let _ = served.await;
}
