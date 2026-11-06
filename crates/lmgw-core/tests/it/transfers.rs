//! Transfers between devices (device transfers design), driven between real
//! paired devices on the device listener (`support::device_pair`), the
//! owner on the plain one, through the router and the gate.
//!
//! - `flow`: the whole flow, and the app id at enrolment;
//! - `access`: targets by app id and never the caller, who reaches what, a
//!   plain API key and the owner on a device's route;
//! - `upload`: chunks, a body that breaks off and its resume, an offset
//!   that is not the resume point, a hash that does not match, a full disk;
//! - `complete`: a file that holds every byte is completed by the next
//!   upload whatever it names, by the status form, by the upload that wrote
//!   its last byte however its body ended, and by the sweep;
//! - `stops`: what stops an upload or a download in progress — a cancel, a
//!   disable, an expiry, a body cut mid-block and resumed at once;
//! - `download`: ranges, `If-Range`, 416, ignored ranges, HEAD, a download
//!   before `ready`;
//! - `ends`: `received`, `decline` and `cancel` delete the bytes, the
//!   expiry sweep, a deleted or disabled device's transfers, a device row
//!   gone without its end, a device that pairs again as another app;
//! - `feed`: `transfer.updated` to its parties and the owner only, and
//!   replayed after a reconnect.

use std::path::PathBuf;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::realtime_chat_thread::{world, World};
use crate::support::device_pair::{pair_as, Paired};

mod access;
mod complete;
mod download;
mod ends;
mod feed;
mod flow;
mod stops;
mod upload;

/// The app every test device declares, unless it declares another.
pub(crate) const APP: &str = "dev.example.storyplayer";

/// A world with two devices of [`APP`]: the sender and the receiver.
pub(crate) async fn two() -> (World, Paired, Paired) {
    let w = world(|_| {}).await;
    let desktop = pair_as(&w.state, &w.gw, "desktop", json!({}), Some(APP)).await;
    let phone = pair_as(&w.state, &w.gw, "phone", json!({}), Some(APP)).await;
    (w, desktop, phone)
}

/// A device named `name` of app `app` (`None`: none) on `w`.
pub(crate) async fn device(w: &World, name: &str, app: Option<&str>) -> Paired {
    pair_as(&w.state, &w.gw, name, json!({}), app).await
}

/// The bytes' sha256, lowercase hex.
pub(crate) fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A test file of `n` bytes that is not all one byte.
pub(crate) fn file(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

/// `method path` as `client` with an optional JSON body: the status and the
/// JSON answer (`Null` when it is none).
pub(crate) async fn call(
    w: &World,
    client: &reqwest::Client,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req = client.request(method.parse().unwrap(), format!("{}{path}", w.gw));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `from` offers `bytes` to `to` as [`APP`]: the transfer (201).
pub(crate) async fn offer(w: &World, from: &Paired, to: &Paired, bytes: &[u8]) -> Value {
    let (s, v) = call(
        w,
        &from.client,
        "POST",
        "/transfers/api/transfers",
        Some(json!({
            "to_device": to.id,
            "app_id": APP,
            "kind": "playthrough",
            "title": "Chapter 3 — the lighthouse",
            "filename": "chapter-3.zip",
            "size": bytes.len(),
            "sha256": sha(bytes),
        })),
    )
    .await;
    assert_eq!(s, 201, "{v}");
    v
}

/// `PUT …/content` of `bytes` as the range from byte `first` of a file of
/// `size` bytes: the status and the answer.
pub(crate) async fn put(
    w: &World,
    client: &reqwest::Client,
    id: &str,
    first: u64,
    bytes: &[u8],
    size: u64,
) -> (u16, Value) {
    let last = first + bytes.len() as u64 - 1;
    let resp = client
        .put(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .header("content-range", format!("bytes {first}-{last}/{size}"))
        .header("content-type", "application/octet-stream")
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `from` offers `bytes` to `to` and uploads them whole: the transfer's id,
/// `ready`.
pub(crate) async fn ready(w: &World, from: &Paired, to: &Paired, bytes: &[u8]) -> String {
    let t = offer(w, from, to, bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let (s, v) = put(w, &from.client, &id, 0, bytes, bytes.len() as u64).await;
    assert_eq!((s, v["state"].as_str()), (200, Some("ready")), "{v}");
    id
}

/// Transfer `id` as `client` reads it.
pub(crate) async fn read(w: &World, client: &reqwest::Client, id: &str) -> (u16, Value) {
    call(
        w,
        client,
        "GET",
        &format!("/transfers/api/transfers/{id}"),
        None,
    )
    .await
}

/// Where transfer `id`'s bytes live on `w`'s disk.
pub(crate) fn path_of(w: &World, id: &str) -> PathBuf {
    lmgw_core::transfers::dir(&w.state.data_dir).join(id)
}

/// Transfer `id`'s file written whole with `bytes` behind lmgw's back: what
/// an upload whose last write was not followed by its completion leaves.
pub(crate) fn left_full(w: &World, id: &str, bytes: &[u8]) {
    std::fs::create_dir_all(lmgw_core::transfers::dir(&w.state.data_dir)).unwrap();
    std::fs::write(path_of(w, id), bytes).unwrap();
}

/// `PUT …/content` with `Content-Range: bytes */<size>` and no body: where
/// the upload stands, and its completion when lmgw holds every byte.
pub(crate) async fn status(
    w: &World,
    client: &reqwest::Client,
    id: &str,
    size: u64,
) -> (u16, Value) {
    let resp = client
        .put(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .header("content-range", format!("bytes */{size}"))
        .header("content-length", "0")
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `PUT …/content` naming the whole file of `size` bytes whose body is
/// `first`, then a body that never ends: the request in a task, answered
/// when something stops it.
pub(crate) fn stalled(
    w: &World,
    client: &reqwest::Client,
    id: &str,
    first: Vec<u8>,
    size: u64,
) -> tokio::task::JoinHandle<reqwest::Result<reqwest::Response>> {
    let body = futures::stream::unfold(Some(first), |part| async move {
        match part {
            Some(p) => Some((Ok::<_, std::io::Error>(bytes::Bytes::from(p)), None)),
            None => std::future::pending().await,
        }
    });
    let req = client
        .put(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .header("content-range", format!("bytes 0-{}/{size}", size - 1))
        .body(reqwest::Body::wrap_stream(body));
    tokio::spawn(async move { req.send().await })
}

/// Read transfer `id` as `client` until `done` holds of it; fail after
/// 10 s. The transfer as it then is.
pub(crate) async fn until(
    w: &World,
    client: &reqwest::Client,
    id: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (_, t) = read(w, client, id).await;
        if done(&t) {
            return t;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "transfer {id} stayed {t}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
