//! The download (device transfers design §5): one range answers 206,
//! `If-Range` with the ETag serves it and with anything else the whole
//! file, a range that names no byte of the file is 416, a `Range` lmgw does
//! not read is ignored (I27), HEAD answers the whole file's head alone,
//! whatever its `Range`, and a download before `ready` is 409 with the
//! state. No download's file stays open once its body was read.

use super::{call, file, offer, ready, sha, two};

/// GET the content as `client` with `headers`: the status, the headers and
/// the body.
async fn get(
    w: &crate::realtime_chat_thread::World,
    client: &reqwest::Client,
    id: &str,
    headers: &[(&str, &str)],
) -> (u16, reqwest::header::HeaderMap, Vec<u8>) {
    let mut req = client.get(format!("{}/transfers/api/transfers/{id}/content", w.gw));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let h = resp.headers().clone();
    (status, h, resp.bytes().await.unwrap().to_vec())
}

#[tokio::test]
async fn a_range_is_served_partial_and_if_range_guards_it() {
    let (w, desktop, phone) = two().await;
    let bytes = file(10_000);
    let id = ready(&w, &desktop, &phone, &bytes).await;
    let etag = format!("\"sha256:{}\"", sha(&bytes));

    let (s, h, body) = get(&w, &phone.client, &id, &[("range", "bytes=1000-1999")]).await;
    assert_eq!(s, 206);
    assert_eq!(h["content-range"], "bytes 1000-1999/10000");
    assert_eq!(h["content-length"], "1000");
    assert_eq!(body, &bytes[1000..2000]);

    let (s, h, body) = get(&w, &phone.client, &id, &[("range", "bytes=9000-")]).await;
    assert_eq!(
        (s, h["content-range"].to_str().unwrap()),
        (206, "bytes 9000-9999/10000")
    );
    assert_eq!(body, &bytes[9000..]);
    let (s, _, body) = get(&w, &phone.client, &id, &[("range", "bytes=-10")]).await;
    assert_eq!((s, body.as_slice()), (206, &bytes[9990..]));

    let (s, _, body) = get(
        &w,
        &phone.client,
        &id,
        &[("range", "bytes=5000-"), ("if-range", &etag)],
    )
    .await;
    assert_eq!((s, body.as_slice()), (206, &bytes[5000..]));
    let (s, h, body) = get(
        &w,
        &phone.client,
        &id,
        &[("range", "bytes=5000-"), ("if-range", "\"sha256:00\"")],
    )
    .await;
    assert_eq!(s, 200, "another ETag: the whole file");
    assert!(h.get("content-range").is_none());
    assert_eq!(body.len(), 10_000);

    // The unit is read case-insensitively (RFC 9110 §14.1).
    let (s, _, body) = get(&w, &phone.client, &id, &[("range", "Bytes=0-9")]).await;
    assert_eq!((s, body.as_slice()), (206, &bytes[..10]));
    // One byte range that names no byte of the file: 416.
    for bad in [
        "bytes=10000-",
        "bytes=-0",
        "bytes=20000-20010",
        "bytes=99999999999999999999999-",
    ] {
        let (s, h, body) = get(&w, &phone.client, &id, &[("range", bad)]).await;
        assert_eq!(s, 416, "{bad}");
        assert_eq!(h["content-range"], "bytes */10000", "{bad}");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], "range_not_satisfiable", "{bad}");
    }
    // A Range lmgw does not read is ignored (RFC 9110 §14.2): the whole
    // file, 200 — another unit, several ranges, an invalid one. A number
    // past u64 is u64::MAX: a last position clips (206), a first is 416 above.
    for ignored in ["items=0-5", "bytes=0-1,5-6", "bytes=5-1", "bytes=x"] {
        let (s, h, body) = get(&w, &phone.client, &id, &[("range", ignored)]).await;
        assert_eq!(s, 200, "{ignored}");
        assert!(h.get("content-range").is_none(), "{ignored}");
        assert_eq!(body, bytes, "{ignored}");
    }
    let (s, h, body) = get(
        &w,
        &phone.client,
        &id,
        &[("range", "bytes=9990-99999999999999999999999")],
    )
    .await;
    assert_eq!((s, body.as_slice()), (206, &bytes[9990..]));
    assert_eq!(h["content-range"], "bytes 9990-9999/10000");
    // Every body above was read to its end: no file is held for them.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while w.state.transfers.downloads_open_for_tests() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a download's file stays open"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_download_before_ready_is_refused_with_the_state() {
    let (w, desktop, phone) = two().await;
    let bytes = file(100);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap();
    let (s, _, body) = get(&w, &phone.client, id, &[]).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(s, 409, "{v}");
    assert_eq!(
        (v["code"].as_str(), v["state"].as_str()),
        (Some("transfer_state"), Some("uploading"))
    );
    for action in ["received", "decline"] {
        let (s, v) = call(
            &w,
            &phone.client,
            "POST",
            &format!("/transfers/api/transfers/{id}/{action}"),
            None,
        )
        .await;
        assert_eq!(
            (s, v["state"].as_str()),
            (409, Some("uploading")),
            "{action}: {v}"
        );
    }
}

/// HEAD on the content route answers the whole file's head, without the
/// body: the receiver's 200 with the file's length and ETag, a `Range` or
/// `If-Range` ignored (RFC 9110 §14.2, 2026-10-22; it was the range's 206
/// before), and another device's 404.
#[tokio::test]
async fn a_head_of_the_content_answers_its_head_only() {
    let (w, desktop, phone) = two().await;
    let tablet = super::device(&w, "tablet", Some(super::APP)).await;
    let bytes = file(5000);
    let id = ready(&w, &desktop, &phone, &bytes).await;
    let url = format!("{}/transfers/api/transfers/{id}/content", w.gw);
    let resp = phone.client.head(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let h = resp.headers().clone();
    assert_eq!(h["content-length"], "5000");
    assert_eq!(
        h["etag"].to_str().unwrap(),
        format!("\"sha256:{}\"", sha(&bytes))
    );
    assert_eq!(h["accept-ranges"], "bytes");
    assert!(resp.bytes().await.unwrap().is_empty(), "no body");
    for (range, if_range) in [
        ("bytes=10-19", None),
        ("bytes=10-19", Some(h["etag"].to_str().unwrap())),
        ("bytes=9000-", None),
    ] {
        let mut req = phone.client.head(&url).header("range", range);
        if let Some(v) = if_range {
            req = req.header("if-range", v);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), 200, "{range}");
        assert_eq!(resp.headers()["content-length"], "5000", "{range}");
        assert!(resp.headers().get("content-range").is_none(), "{range}");
        assert_eq!(resp.headers()["accept-ranges"], "bytes", "{range}");
    }
    assert_eq!(
        w.state.transfers.downloads_open_for_tests(),
        0,
        "a HEAD opens no file"
    );
    let resp = tablet.client.head(&url).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

/// WP13: a transfer's bytes come from a peer, so the download sends nosniff
/// and the sandbox policy on the 200, the 206 and the 416 (its
/// `Content-Disposition: attachment` stays).
#[tokio::test]
async fn a_download_is_sandboxed_on_200_206_and_416() {
    use crate::support::stored::assert_sandboxed;
    let (w, desktop, phone) = two().await;
    let bytes = file(10_000);
    let id = ready(&w, &desktop, &phone, &bytes).await;
    for (range, want) in [
        (None, 200),
        (Some("bytes=10-19"), 206),
        (Some("bytes=20000-"), 416),
    ] {
        let hs: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
        let (s, h, _) = get(&w, &phone.client, &id, &hs).await;
        assert_eq!(s, want, "{range:?}");
        assert_sandboxed(&h, &format!("{range:?}"));
        if want != 416 {
            assert!(h["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment"));
        }
    }
}
