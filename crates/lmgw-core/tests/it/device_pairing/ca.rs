//! Rotating the device CA (device link design §5.5, D19): a new pin, every
//! enrolled device reset to wait for a new token, every token alive ended,
//! a device under the old CA no longer verifies lmgw, and lmgw no longer
//! takes a certificate the old CA issued.

use std::sync::Arc;

use serde_json::json;

use super::world;
use crate::support::device_pair;

#[tokio::test]
async fn rotating_the_ca_resets_every_device_and_ends_every_token() {
    let w = world().await;
    let phone = device_pair::pair(&w.state, &w.gw, "phone", json!({ "rpm_limit": 7 })).await;
    let (_, waiting) = w.waiting("tablet").await;
    let before = w.state.devices.trust.ca().unwrap().fp().to_string();

    let (s, v) = w.op("device_ca_rotate", json!({})).await;
    assert_eq!(s, 200, "{v}");
    assert_ne!(v["fp"], json!(before), "{v}");
    assert_eq!(v["devices_reset"], json!(["phone"]), "{v}");
    assert_eq!(
        Some(v["fp"].as_str().unwrap().to_string()),
        w.state.devices.trust.ca().map(|ca| ca.fp().to_string())
    );

    // The phone's row waits for a new token, its policy kept.
    let row = w.row("phone").await;
    assert_eq!(row["pairing"]["state"], json!("waiting"), "{row}");
    assert_eq!(row["rpm_limit"], json!(7));
    // Every token ended: no handshake without a certificate.
    assert_eq!(lmgw_core::devices::status(&w.state).pairing.tokens_alive, 0);
    assert!(
        w.row("tablet").await["pairing"]["token"]["ended_at"].is_string(),
        "the waiting device's token ended"
    );
    drop(waiting);

    // The phone, under the old CA, refuses lmgw before it sends anything.
    let cfg = Arc::new(device_pair::device_config(
        &phone.credentials,
        &phone.identity,
    ));
    let port = device_pair::listening(&w.state).await;
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let e = tokio_rustls::TlsConnector::from(cfg)
        .connect(
            rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into()),
            tcp,
        )
        .await
        .unwrap_err();
    let tls = lmgw_client::tls::find_tls_error(&e).expect("a TLS error");
    assert_eq!(
        lmgw_client::tls::TlsRefusal::of(tls),
        Some(lmgw_client::tls::TlsRefusal::AuthorityChanged)
    );

    // The other way: a client that trusts the new CA, and so verifies lmgw,
    // but presents its certificate from the old one is refused by lmgw. TLS
    // 1.3 refuses a client after its Finished, so the refusal arrives at
    // the first read, if not at the handshake.
    let mut trusting = phone.credentials.clone();
    trusting.ca_certificate = w.state.devices.trust.ca().unwrap().certificate().to_vec();
    let cfg = Arc::new(device_pair::device_config(&trusting, &phone.identity));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let refused = match tokio_rustls::TlsConnector::from(cfg)
        .connect(
            rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into()),
            tcp,
        )
        .await
    {
        Err(e) => e,
        Ok(mut tls) => {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            tls.write_all(b"GET /chat/api/threads HTTP/1.1\r\nhost: lmgw\r\n\r\n")
                .await
                .unwrap();
            let mut buf = [0u8; 64];
            match tls.read(&mut buf).await {
                Err(e) => e,
                Ok(n) => panic!("lmgw answered {n} bytes to the old CA's certificate"),
            }
        }
    };
    let tls = lmgw_client::tls::find_tls_error(&refused)
        .unwrap_or_else(|| panic!("not a TLS refusal: {refused}"));
    assert_eq!(
        lmgw_client::tls::TlsRefusal::of(tls),
        Some(lmgw_client::tls::TlsRefusal::CertificateRefused),
        "{tls:?}"
    );

    // Paired again under the new CA, it is served.
    let link = device_pair::mint(&w.state, &w.gw, "device_token", json!({ "id": phone.id })).await;
    assert!(
        link.contains(&v["fp"].as_str().unwrap().replace(':', "%3A")),
        "{link}"
    );
    let enrolled = device_pair::enrol(&link).await;
    device_pair::confirm(&w.gw, &enrolled).await;
    let phone = device_pair::connected(enrolled, "device:phone".into()).await;
    let resp = phone
        .client
        .get(format!("{}/chat/api/threads", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
