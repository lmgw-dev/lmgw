//! The device listener: its own hyper connection over TLS, the same relay.
//! A device's client that goes mid-stream is heard as the plain listener's
//! is, with the stream writing it nothing.

use futures::StreamExt;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::store::{NewAlias, NewUpstream};

use super::*;
use crate::support::device_link::gateway_with;

#[tokio::test]
async fn a_devices_stream_that_writes_it_nothing_stops_when_it_goes() {
    use crate::support::sockopts::keepalives;
    // A keep-alive no other test's socket carries.
    let gw = gateway_with(|s| s.chat_feed_keepalive_s = 29).await;
    let (port, model) = model(Says::Nothing).await;
    let up = store::insert_upstream(
        &gw.state.db,
        &NewUpstream {
            name: "remote".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 600_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &gw.state.db,
        &NewAlias {
            alias: "m".into(),
            upstream_id: up,
            upstream_model_id: "remote-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    gw.state.reload_snapshot().await.unwrap();
    let device = gw.device("phone", true).await;

    let resp = gw
        .https(Some(&device.identity))
        .post(gw.url("/v1/chat/completions"))
        .json(&chat_body(true, json_schema()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.expect("the stream opens").unwrap();
    assert!(!first.is_empty());
    // While it streams, its connection carries `TCP_USER_TIMEOUT` at twice
    // the keep-alive, beside the accept-time keep-alive (device link
    // design §4.3, 2026-10-24).
    let streaming = keepalives();
    assert!(
        streaming
            .iter()
            .any(|k| k.on && k.idle_s == 29 && k.interval_s == 29 && k.user_timeout_ms == 58_000),
        "{streaming:?}"
    );
    drop(stream);
    model_stops(&model, Instant::now()).await;
    let row = newest_row(&gw.state).await;
    assert_eq!(row.error_kind.as_deref(), Some("canceled"), "{row:?}");
    // Ended, it is gone again: the connection has its accept-time timers,
    // if it is open at all.
    crate::common::patience::until("the stream's TCP_USER_TIMEOUT is gone", || {
        keepalives().iter().all(|k| k.user_timeout_ms != 58_000)
    })
    .await;
}
