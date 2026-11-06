//! A file that holds every byte is completed (device transfers design §4,
//! I25), whoever finds it so: the next upload of it, whatever range it
//! names, and the status form `bytes */<size>`; the upload that wrote the
//! last byte, however its body ended after it; the sweep, for one nobody
//! finished. Never stuck `uploading` with every byte there.

use std::time::Duration;

use serde_json::json;

use super::{call, file, left_full, offer, path_of, put, read, sha, stalled, status, two, until};

/// lmgw stopped between an upload's last write and its completion: the
/// file is whole, the transfer uploads still. The next upload completes
/// it, whatever its range — and the status form asks only that.
#[tokio::test]
async fn the_next_upload_completes_a_full_file_whatever_it_names() {
    let (w, desktop, phone) = two().await;
    let bytes = file(1000);
    for ask in ["status", "the last range again", "the first range"] {
        let t = offer(&w, &desktop, &phone, &bytes).await;
        let id = t["id"].as_str().unwrap();
        left_full(&w, id, &bytes);
        let (_, t) = read(&w, &desktop.client, id).await;
        assert_eq!(
            (t["state"].as_str(), t["received"].as_u64()),
            (Some("uploading"), Some(1000)),
            "{ask}: {t}"
        );
        let (s, v) = match ask {
            "status" => status(&w, &desktop.client, id, 1000).await,
            "the last range again" => put(&w, &desktop.client, id, 900, &bytes[900..], 1000).await,
            _ => put(&w, &desktop.client, id, 0, &bytes[..10], 1000).await,
        };
        assert_eq!(s, 200, "{ask}: {v}");
        assert_eq!(v, json!({"received": 1000, "state": "ready"}), "{ask}");
        // Done: a further one is told so.
        let (s, v) = status(&w, &desktop.client, id, 1000).await;
        assert_eq!(
            (s, v["code"].as_str(), v["state"].as_str()),
            (409, Some("transfer_state"), Some("ready")),
            "{ask}: {v}"
        );
    }
    // Bytes that are not the offer's fail as the last range's would.
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap();
    let mut other = bytes.clone();
    other[3] ^= 0x55;
    left_full(&w, id, &other);
    let (s, v) = status(&w, &desktop.client, id, 1000).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (422, Some("transfer_hash_mismatch")),
        "{v}"
    );
    assert!(!path_of(&w, id).exists());
}

/// The status form on an upload that is not whole says where it stands
/// and moves nothing; with a body, or another total, it is 400.
#[tokio::test]
async fn the_status_form_says_where_an_upload_stands() {
    let (w, desktop, phone) = two().await;
    let bytes = file(1000);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap();
    let (s, v) = status(&w, &desktop.client, id, 1000).await;
    assert_eq!((s, v), (200, json!({"received": 0, "state": "uploading"})));
    put(&w, &desktop.client, id, 0, &bytes[..300], 1000).await;
    let (s, v) = status(&w, &desktop.client, id, 1000).await;
    assert_eq!(
        (s, v),
        (200, json!({"received": 300, "state": "uploading"}))
    );
    let (s, v) = status(&w, &desktop.client, id, 999).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("bad_request")), "{v}");
    let resp = desktop
        .client
        .put(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .header("content-range", "bytes */1000")
        .body(bytes[300..].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "the status form carries no body");
    let (_, t) = read(&w, &desktop.client, id).await;
    assert_eq!(t["received"], 300);
}

/// The upload that wrote the last byte completes, however its body ended
/// after it: one that went on past its range (a chunked body; the range is
/// written, the rest is not), one that broke off after its last byte.
#[tokio::test]
async fn the_last_byte_completes_however_the_body_ends() {
    let (w, desktop, phone) = two().await;
    let bytes = file(1000);
    let url = |id: &str| format!("{}/transfers/api/transfers/{id}/content", w.gw);

    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let long = futures::stream::iter([
        Ok::<_, std::io::Error>(bytes::Bytes::from(bytes.clone())),
        Ok(bytes::Bytes::from_static(b"and more")),
    ]);
    let resp = desktop
        .client
        .put(url(&id))
        .header("content-range", "bytes 0-999/1000")
        .body(reqwest::Body::wrap_stream(long))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v, json!({"received": 1000, "state": "ready"}));
    assert_eq!(std::fs::read(path_of(&w, &id)).unwrap(), bytes);

    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let whole = bytes.clone();
    let broke = futures::stream::unfold(0u8, move |n| {
        let whole = whole.clone();
        async move {
            match n {
                0 => Some((Ok::<_, std::io::Error>(bytes::Bytes::from(whole)), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Some((Err(std::io::Error::other("the radio went")), 2))
                }
                _ => None,
            }
        }
    });
    let sent = desktop
        .client
        .put(url(&id))
        .header("content-range", "bytes 0-999/1000")
        .body(reqwest::Body::wrap_stream(broke))
        .send()
        .await;
    assert!(sent.is_err() || !sent.unwrap().status().is_success());
    let t = until(&w, &desktop.client, &id, |t| t["state"] != "uploading").await;
    assert_eq!(
        t["state"], "ready",
        "nobody listened, and it completed: {t}"
    );
}

/// The sweep, which also runs as lmgw starts, completes an upload nobody
/// finished: ready (offered to the receiver), or failed when the bytes are
/// not the offer's.
#[tokio::test]
async fn the_sweep_completes_an_upload_left_full() {
    let (w, desktop, phone) = two().await;
    let bytes = file(4096);
    let good = offer(&w, &desktop, &phone, &bytes).await;
    let good = good["id"].as_str().unwrap().to_string();
    left_full(&w, &good, &bytes);
    let bad = offer(&w, &desktop, &phone, &bytes).await;
    let bad = bad["id"].as_str().unwrap().to_string();
    left_full(&w, &bad, &file(4096).iter().map(|b| !b).collect::<Vec<_>>());
    let half = offer(&w, &desktop, &phone, &bytes).await;
    let half = half["id"].as_str().unwrap().to_string();
    put(&w, &desktop.client, &half, 0, &bytes[..100], 4096).await;
    lmgw_core::transfers::upkeep(&w.state).await;
    let (_, t) = read(&w, &desktop.client, &good).await;
    assert_eq!(t["state"], "ready", "{t}");
    let (_, l) = call(
        &w,
        &phone.client,
        "GET",
        "/transfers/api/transfers?role=incoming",
        None,
    )
    .await;
    assert_eq!(l["transfers"][0]["id"], good.as_str(), "offered: {l}");
    let got = phone
        .client
        .get(format!("{}/transfers/api/transfers/{good}/content", w.gw))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(sha(&got), sha(&bytes));
    let (_, t) = read(&w, &desktop.client, &bad).await;
    assert_eq!(t["state"], "failed", "{t}");
    assert!(!path_of(&w, &bad).exists());
    let (_, t) = read(&w, &desktop.client, &half).await;
    assert_eq!(
        (t["state"].as_str(), t["received"].as_u64()),
        (Some("uploading"), Some(100)),
        "one not whole goes on: {t}"
    );
}

/// An upload that wrote the last byte and still reads its body is left to
/// complete the file (2026-10-22): the sweep passes over it, stopping
/// nothing, and a newer upload that stops it finds it completed — the
/// stopped one answers as the completion does, not 409 `transfer_offset`
/// with every byte received.
#[tokio::test]
async fn an_upload_that_wrote_the_last_byte_completes_when_stopped() {
    let (w, desktop, phone) = two().await;
    let bytes = file(1000);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let job = stalled(&w, &desktop.client, &id, bytes.clone(), 1000);
    until(&w, &desktop.client, &id, |t| t["received"] == 1000).await;
    lmgw_core::transfers::upkeep(&w.state).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!job.is_finished(), "the sweep stopped the upload");
    let (_, t) = read(&w, &desktop.client, &id).await;
    assert_eq!(t["state"], "uploading", "{t}");
    // A newer upload stops it: it completes the file, and the newer one
    // finds it ready.
    let (s, v) = status(&w, &desktop.client, &id, 1000).await;
    assert_eq!(
        (s, v["code"].as_str(), v["state"].as_str()),
        (409, Some("transfer_state"), Some("ready")),
        "{v}"
    );
    let resp = tokio::time::timeout(Duration::from_secs(10), job)
        .await
        .expect("the stopped upload is answered")
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v, json!({"received": 1000, "state": "ready"}));
    assert_eq!(w.state.transfers.upload_slots_for_tests(), 0);
}
