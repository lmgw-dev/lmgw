//! The upload (device transfers design §3, §4): chunks, a body that breaks
//! off and the resume from what lmgw kept, an offset that is not the
//! resume point, a hash that does not match, the space check.

use std::time::Duration;

use serde_json::json;

use super::{call, file, offer, path_of, put, read, sha, two, APP};

/// Three chunks, each from the last one's end; the last makes it ready.
#[tokio::test]
async fn chunks_append_until_the_last_one_makes_it_ready() {
    let (w, desktop, phone) = two().await;
    let bytes = file(100_000);
    let size = bytes.len() as u64;
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap();
    let mut at = 0usize;
    for (n, len) in [40_000usize, 40_000, 20_000].into_iter().enumerate() {
        let (s, v) = put(
            &w,
            &desktop.client,
            id,
            at as u64,
            &bytes[at..at + len],
            size,
        )
        .await;
        at += len;
        let state = if n == 2 { "ready" } else { "uploading" };
        assert_eq!(s, 200, "{v}");
        assert_eq!(v, json!({"received": at, "state": state}));
        let (_, t) = read(&w, &desktop.client, id).await;
        assert_eq!(t["received"], at);
    }
}

/// A body that breaks off keeps what reached lmgw: `received` is where the
/// upload goes on, and it does.
#[tokio::test]
async fn a_body_that_breaks_off_resumes_where_lmgw_kept_it() {
    let (w, desktop, phone) = two().await;
    let bytes = file(200_000);
    let size = bytes.len() as u64;
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let half = bytes[..120_000].to_vec();
    // The first part, a pause that lets it reach lmgw, then the break.
    let pieces = futures::stream::unfold(0u8, move |n| {
        let half = half.clone();
        async move {
            match n {
                0 => Some((Ok::<_, std::io::Error>(bytes::Bytes::from(half)), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    Some((Err(std::io::Error::other("the radio went")), 2))
                }
                _ => None,
            }
        }
    });
    let sent = desktop
        .client
        .put(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .header("content-range", format!("bytes 0-{}/{size}", size - 1))
        .body(reqwest::Body::wrap_stream(pieces))
        .send()
        .await;
    assert!(sent.is_err() || !sent.unwrap().status().is_success());
    // The server writes what it got, then answers nobody: read until it did.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let kept = loop {
        let (_, t) = read(&w, &desktop.client, &id).await;
        let r = t["received"].as_u64().unwrap();
        if r == 120_000 || tokio::time::Instant::now() > deadline {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(kept, 120_000, "what reached lmgw is kept");
    assert_eq!(std::fs::metadata(path_of(&w, &id)).unwrap().len(), 120_000);
    let (s, v) = put(
        &w,
        &desktop.client,
        &id,
        kept,
        &bytes[kept as usize..],
        size,
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (200, Some("ready")), "{v}");
}

/// A range that does not start at the resume point is 409 with it; a total
/// that is not the offer's, or a Content-Length that is not the range's,
/// is 400 and moves nothing.
#[tokio::test]
async fn a_range_starts_at_the_resume_point() {
    let (w, desktop, phone) = two().await;
    let bytes = file(1000);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap();
    let (s, _) = put(&w, &desktop.client, id, 0, &bytes[..300], 1000).await;
    assert_eq!(s, 200);
    let (s, v) = put(&w, &desktop.client, id, 500, &bytes[500..], 1000).await;
    assert_eq!(s, 409, "{v}");
    assert_eq!(
        (v["code"].as_str(), v["received"].as_u64()),
        (Some("transfer_offset"), Some(300))
    );
    let (s, v) = put(&w, &desktop.client, id, 300, &bytes[300..400], 999).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("bad_request")), "{v}");
    let resp = desktop
        .client
        .put(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .body(bytes[300..].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "no Content-Range");
    let (_, t) = read(&w, &desktop.client, id).await;
    assert_eq!(t["received"], 300);
    // The receiver does not upload.
    let (s, v) = put(&w, &phone.client, id, 300, &bytes[300..], 1000).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (404, Some("transfer_not_found")),
        "{v}"
    );
}

/// Bytes whose sha256 is not the offer's: 422, failed, the bytes deleted,
/// and a further upload is 409 with the state.
#[tokio::test]
async fn a_hash_that_does_not_match_fails_the_transfer() {
    let (w, desktop, phone) = two().await;
    let bytes = file(500);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap();
    let mut other = bytes.clone();
    other[17] ^= 0xff;
    let (s, v) = put(&w, &desktop.client, id, 0, &other, 500).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (422, Some("transfer_hash_mismatch")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains(&sha(&other)), "{v}");
    let (_, t) = read(&w, &desktop.client, id).await;
    assert_eq!(t["state"], "failed");
    // A sentence for people; the refusal named the hashes.
    assert_eq!(
        t["end_reason"],
        "the uploaded bytes did not match the sha256 the offer named"
    );
    assert!(!path_of(&w, id).exists());
    let (s, v) = put(&w, &desktop.client, id, 0, &bytes, 500).await;
    assert_eq!(
        (s, v["code"].as_str(), v["state"].as_str()),
        (409, Some("transfer_state"), Some("failed")),
        "{v}"
    );
}

/// The only cap is the free space less what uploads in progress still
/// need: an offer above it is 507 with `free_bytes`, and the targets say
/// the same number.
#[tokio::test]
async fn an_offer_larger_than_the_free_space_is_refused_with_it() {
    let (w, desktop, phone) = two().await;
    w.state.transfers.set_free_bytes_for_tests(Some(1000));
    let first = file(600);
    let t = offer(&w, &desktop, &phone, &first).await;
    let (_, v) = call(
        &w,
        &desktop.client,
        "GET",
        &format!("/transfers/api/targets?app_id={APP}"),
        None,
    )
    .await;
    assert_eq!(v["free_bytes"], 400, "the first offer still needs 600: {v}");
    let second = file(500);
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        "/transfers/api/transfers",
        Some(
            json!({"to_device": phone.id, "app_id": APP, "kind": "save", "filename": "b.bin",
                    "size": 500, "sha256": sha(&second)}),
        ),
    )
    .await;
    assert_eq!(s, 507, "{v}");
    assert_eq!(
        (v["code"].as_str(), v["free_bytes"].as_u64()),
        (Some("transfer_storage_full"), Some(400))
    );
    // Canceled, the first frees its share.
    let id = t["id"].as_str().unwrap();
    let (s, _) = call(
        &w,
        &desktop.client,
        "POST",
        &format!("/transfers/api/transfers/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(s, 200);
    let (_, v) = call(
        &w,
        &desktop.client,
        "GET",
        &format!("/transfers/api/targets?app_id={APP}"),
        None,
    )
    .await;
    assert_eq!(v["free_bytes"], 1000, "{v}");
}

/// A retried upload stops the one its client gave up on: the stalled one
/// is answered 409 with what lmgw holds, the new one goes on from there
/// (I9).
#[tokio::test]
async fn a_newer_upload_stops_a_stalled_one() {
    let (w, desktop, phone) = two().await;
    let bytes = file(50_000);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let first = bytes[..20_000].to_vec();
    // 20 000 bytes, then a body that never ends.
    let stalled = futures::stream::unfold(Some(first), |part| async move {
        match part {
            Some(p) => Some((Ok::<_, std::io::Error>(bytes::Bytes::from(p)), None)),
            None => std::future::pending().await,
        }
    });
    let client = desktop.client.clone();
    let url = format!("{}/transfers/api/transfers/{id}/content", w.gw);
    let old = tokio::spawn(async move {
        client
            .put(url)
            .header("content-range", "bytes 0-49999/50000")
            .body(reqwest::Body::wrap_stream(stalled))
            .send()
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (_, t) = read(&w, &desktop.client, &id).await;
        if t["received"] == 20_000 || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (s, v) = put(&w, &desktop.client, &id, 20_000, &bytes[20_000..], 50_000).await;
    assert_eq!((s, v["state"].as_str()), (200, Some("ready")), "{v}");
    let old = tokio::time::timeout(Duration::from_secs(10), old)
        .await
        .expect("the stalled upload is answered")
        .unwrap()
        .unwrap();
    assert_eq!(old.status(), 409);
    let v: serde_json::Value = old.json().await.unwrap();
    assert_eq!(
        (v["code"].as_str(), v["received"].as_u64()),
        (Some("transfer_offset"), Some(20_000))
    );
}
