//! The status block's trust half (§9.2) over the real router: missing until
//! the CA is made, then its pin, creation day and key mode, and one line per
//! server certificate.

use lmgw_api_types::device_link::{DeviceCaKeyMode, DeviceCaState, DeviceLeafSlot, DevicesStatus};
use lmgw_core::state::AppState;
use serde_json::Value;

use crate::common::{self, Gw};

async fn devices(gw: &Gw) -> DevicesStatus {
    let r = gw
        .client()
        .get(format!("{gw}/api/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let live: Value = r.json().await.unwrap();
    common::round_trips("status devices", &live["devices"])
}

#[tokio::test]
async fn the_status_carries_the_device_ca_and_its_server_certificates() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;

    let st = devices(&gw).await;
    assert_eq!(st.ca.state, DeviceCaState::Missing);
    assert_eq!(
        (st.ca.fp.as_deref(), st.ca.key_mode, st.ca.error.as_ref()),
        (None, None, None)
    );
    // Relative to the data directory: no caller learns the home directory.
    assert_eq!(
        (st.ca.key_file.as_str(), st.ca.cert_file.as_str()),
        ("tls/ca.key", "tls/ca.crt")
    );
    assert_eq!(st.ca.certificate_pem, None);
    assert!(st.leaves.is_empty());

    let ca = state
        .devices
        .trust
        .open(&state.data_dir, true)
        .await
        .unwrap();
    let st = devices(&gw).await;
    assert_eq!(st.ca.state, DeviceCaState::Ready);
    assert_eq!(st.ca.fp.as_deref(), Some(ca.fp()));
    // The public certificate, as `curl --cacert` takes it.
    assert_eq!(
        st.ca.certificate_pem.as_deref(),
        Some(ca.certificate_pem().as_str())
    );
    // A test gateway has no Secret Service: plain, as a headless start.
    assert_eq!(st.ca.key_mode, Some(DeviceCaKeyMode::Plain));
    let today = chrono::Utc::now().format("%Y-%m-%dT00:00:00Z").to_string();
    assert_eq!(st.ca.created.as_deref(), Some(today.as_str()));

    let now = chrono::Utc::now();
    state
        .devices
        .trust
        .leaves
        .leaf(&ca, "127.0.0.1".parse().unwrap(), 30, now)
        .unwrap();
    let st = devices(&gw).await;
    assert_eq!(st.leaves.len(), 1);
    let leaf = &st.leaves[0];
    assert_eq!(leaf.slot, DeviceLeafSlot::Loopback);
    assert_eq!(leaf.address, None);
    assert_eq!(leaf.names, ["127.0.0.1", "::1", "localhost"]);
    let at =
        |t: chrono::DateTime<chrono::Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    assert_eq!(leaf.issued, at(now));
    assert_eq!(leaf.valid_until, at(now + chrono::Duration::days(30)));
    assert_eq!(leaf.renew_after, at(now + chrono::Duration::days(15)));
}
