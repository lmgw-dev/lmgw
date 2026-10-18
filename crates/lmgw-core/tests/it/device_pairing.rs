//! Pairing over the device link (device link design §3, §5.2, §5.5, §12;
//! WP3), driven as a device drives it: the owner's ops on the plain
//! listener, the device's enrolment over TLS pinned to the link's `fp` with
//! `lmgw-client`'s verifier and reader, and its requests with its
//! certificate (`support::device_pair`).
//!
//! - `enrol`: a token's enrolment, the race for one token, a used token
//!   another key presents (contested), and every refusal, failures counted
//!   per address up to the 429;
//! - `repeat`: the same key asking again for the answer it lost, while the
//!   token lives (§3.3's note of 2026-10-15);
//! - `confirm`: an enrolment waits for the owner's Confirm; a re-pair keeps
//!   the old key until the new one enrols; a row paired before TLS pairs
//!   again;
//! - `plain`: no bearer opens a device (D12), and a row paired before TLS
//!   is named;
//! - `ca`: rotating the device CA resets every device and ends every token;
//! - `logs`: a request row says which listener it came in on.

use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};

use crate::common::{self, Gw};
use crate::support::device_pair;

mod ca;
mod confirm;
mod enrol;
mod logs;
mod plain;
mod repeat;

/// A gateway with its device listener on, and the store's own lookup of
/// a certificate's device (no stand-in).
pub(crate) struct World {
    pub state: SharedState,
    pub gw: Gw,
}

pub(crate) async fn world() -> World {
    let state = AppState::init_for_tests().await.unwrap();
    device_pair::listening(&state).await;
    let gw = common::serve(state.clone()).await;
    World { state, gw }
}

impl World {
    pub async fn op(&self, name: &str, body: Value) -> (u16, Value) {
        device_pair::op(&self.gw, name, body).await
    }

    /// A device row for this computer, waiting for its device: its id and
    /// link.
    pub async fn waiting(&self, name: &str) -> (i64, String) {
        let (s, v) = self
            .op(
                "key_create",
                json!({ "kind": "device", "name": name, "local_only": true }),
            )
            .await;
        assert_eq!(s, 200, "{v}");
        (
            v["id"].as_i64().unwrap(),
            v["link"].as_str().unwrap().to_string(),
        )
    }

    /// Device `name`'s row on the Keys list.
    pub async fn row(&self, name: &str) -> Value {
        let v: Value = self
            .gw
            .client()
            .get(format!("{}/api/usage/keys", self.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == json!(format!("device:{name}")))
            .cloned()
            .unwrap_or_else(|| panic!("no device '{name}' in {v}"))
    }
}

/// `body` posted to `/devices/enrol` over TLS pinned to `link`'s `fp`,
/// without a client certificate: the status, the headers and the body.
pub(crate) async fn post_raw(
    link: &str,
    body: String,
) -> (u16, reqwest::header::HeaderMap, String) {
    let parsed = lmgw_client::pairing::PairingLink::parse(link).unwrap();
    let cfg =
        lmgw_client::tls::enrol_config(lmgw_core::devices::tls::provider(), parsed.fp).unwrap();
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(cfg)
        .build()
        .unwrap();
    let resp = client
        .post(format!(
            "{}{}",
            parsed.urls.as_slice()[0],
            lmgw_api_types::device_link::ENROL_PATH
        ))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.text().await.unwrap())
}

/// An enrolment's JSON for `token` and the key `spki`.
pub(crate) fn enrolment(token: &str, spki: &[u8]) -> String {
    use base64::Engine;
    json!({
        "token": token,
        "public_key": base64::engine::general_purpose::STANDARD.encode(spki),
        "client": "lmgw tests",
    })
    .to_string()
}

/// The link's token.
pub(crate) fn token_of(link: &str) -> String {
    lmgw_client::pairing::PairingLink::parse(link)
        .unwrap()
        .token
        .expose()
        .to_string()
}
