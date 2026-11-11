//! What stops an upload or a download in progress (device transfers design
//! §4, §5, I22, I23, I26): a cancel and a disable answer the upload they
//! stop with the state they leave; an upload that finds its transfer
//! overdue expires it; a body cut mid-block and resumed at once loses no
//! byte and shifts none; a download ends with its transfer, and one nobody
//! reads lets go of its file. No upload slot outlives them (I24), and no
//! download's file its transfer (I26).

use std::time::Duration;

use serde_json::{json, Value};

use super::{call, file, offer, path_of, put, read, ready, sha, stalled, two, until, APP};

/// The answer of a stalled upload that something stopped.
async fn answered(
    job: tokio::task::JoinHandle<reqwest::Result<reqwest::Response>>,
) -> (u16, Value) {
    let resp = tokio::time::timeout(Duration::from_secs(10), job)
        .await
        .expect("the stopped upload is answered")
        .unwrap()
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A cancel during an upload stops it, and the upload is answered with the
/// state the cancel left: 409 `transfer_state`, `canceled`, with its
/// reason; the bytes are deleted. A disable of the receiver does the same.
#[tokio::test]
async fn an_end_during_an_upload_answers_it_with_the_state_it_left() {
    let (w, desktop, phone) = two().await;
    let bytes = file(50_000);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let job = stalled(&w, &desktop.client, &id, bytes[..20_000].to_vec(), 50_000);
    until(&w, &desktop.client, &id, |t| t["received"] == 20_000).await;
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        &format!("/transfers/api/transfers/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (200, Some("canceled")), "{v}");
    let (s, v) = answered(job).await;
    assert_eq!(
        (s, v["code"].as_str(), v["state"].as_str()),
        (409, Some("transfer_state"), Some("canceled")),
        "{v}"
    );
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("canceled by device 'desktop'"),
        "{v}"
    );
    assert!(!path_of(&w, &id).exists());

    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let job = stalled(&w, &desktop.client, &id, bytes[..20_000].to_vec(), 50_000);
    until(&w, &desktop.client, &id, |t| t["received"] == 20_000).await;
    let (s, v) =
        crate::device_chat::op(&w, "key_set", json!({"id": phone.id, "enabled": false})).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = answered(job).await;
    assert_eq!(
        (s, v["code"].as_str(), v["state"].as_str()),
        (409, Some("transfer_state"), Some("canceled")),
        "{v}"
    );
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("device 'phone' was disabled"),
        "{v}"
    );
    assert!(!path_of(&w, &id).exists());
    assert_eq!(w.state.transfers.upload_slots_for_tests(), 0);
}

/// An upload of a transfer past its `expires_at` expires it and is told so
/// (409 with the state), at once and without waiting on its own slot
/// (6bc306e9); an upload in progress when it expires is answered the same.
#[tokio::test]
async fn an_overdue_upload_expires_its_transfer() {
    let (w, desktop, phone) = two().await;
    let bytes = file(1000);
    let overdue = |id: String| {
        let db = w.state.db.clone();
        async move {
            sqlx::query("UPDATE transfers SET expires_at = '2000-01-01T00:00:00Z' WHERE id = ?1")
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
        }
    };
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    put(&w, &desktop.client, &id, 0, &bytes[..300], 1000).await;
    overdue(id.clone()).await;
    let (s, v) = tokio::time::timeout(
        Duration::from_secs(10),
        put(&w, &desktop.client, &id, 300, &bytes[300..], 1000),
    )
    .await
    .expect("answered");
    assert_eq!(
        (s, v["code"].as_str(), v["state"].as_str()),
        (409, Some("transfer_state"), Some("expired")),
        "{v}"
    );
    assert!(!path_of(&w, &id).exists());

    // Expired while it writes: the sweep stops it, and it hears why.
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let job = stalled(&w, &desktop.client, &id, bytes[..200].to_vec(), 1000);
    until(&w, &desktop.client, &id, |t| t["received"] == 200).await;
    overdue(id.clone()).await;
    lmgw_core::transfers::upkeep(&w.state).await;
    let (s, v) = answered(job).await;
    assert_eq!(
        (s, v["code"].as_str(), v["state"].as_str()),
        (409, Some("transfer_state"), Some("expired")),
        "{v}"
    );
    assert_eq!(w.state.transfers.upload_slots_for_tests(), 0);
}

/// A body cut in the middle of a large frame, and a resume that comes at
/// once: the resume starts where lmgw really holds the bytes (a write still
/// landing is done before the next upload reads the resume point, and
/// every write is at its offset), so the transfer ends ready with the
/// right bytes.
#[cfg_attr(
    windows,
    ignore = "Windows resets a connection closed with unread bytes and the reset loses the refusal before the client reads it; only a declared oversize /v1 body is drained before its 413 on Windows yet (Windows design §5)"
)]
#[tokio::test]
async fn a_body_cut_mid_block_and_resumed_at_once_ends_ready() {
    let (w, desktop, phone) = two().await;
    let bytes = file(5 << 20);
    let size = bytes.len() as u64;
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    let part = bytes[..3 << 20].to_vec();
    let cut = futures::stream::iter([
        Ok::<_, std::io::Error>(bytes::Bytes::from(part)),
        Err(std::io::Error::other("the radio went")),
    ]);
    let sent = desktop
        .client
        .put(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .header("content-range", format!("bytes 0-{}/{size}", size - 1))
        .body(reqwest::Body::wrap_stream(cut))
        .send()
        .await;
    assert!(sent.is_err() || !sent.unwrap().status().is_success());
    // At once: where it stands, and on from there; a 409 says where lmgw
    // really is.
    let (_, t) = read(&w, &desktop.client, &id).await;
    let mut at = t["received"].as_u64().unwrap();
    let mut tries = 0;
    loop {
        tries += 1;
        let (s, v) = put(&w, &desktop.client, &id, at, &bytes[at as usize..], size).await;
        if s == 200 {
            assert_eq!(v["state"], "ready", "{v}");
            break;
        }
        assert_eq!(
            (s, v["code"].as_str()),
            (409, Some("transfer_offset")),
            "{v}"
        );
        at = v["received"].as_u64().unwrap();
        assert!(tries < 5, "{v}");
    }
    let got = phone
        .client
        .get(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(sha(&got), sha(&bytes));
}

/// A download in progress ends when its transfer does: the body breaks off
/// short of its length once the bytes are deleted.
#[tokio::test]
async fn a_cancel_ends_a_download_in_progress() {
    let (w, desktop, phone) = two().await;
    let bytes = file(64 << 20);
    let size = bytes.len();
    let id = ready(&w, &desktop, &phone, &bytes).await;
    let mut resp = phone
        .client
        .get(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut got = resp.chunk().await.unwrap().unwrap().len();
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        &format!("/transfers/api/transfers/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (200, Some("canceled")), "{v}");
    let ended = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => got += c.len(),
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        }
    })
    .await
    .expect("the body ends");
    assert!(ended.is_err(), "the body breaks off, not ends: {got} bytes");
    assert!(got < size, "{got} of {size} bytes");
    assert_eq!(w.state.transfers.downloads_open_for_tests(), 0);
    // The receiver reads why.
    let (_, t) = read(&w, &phone.client, &id).await;
    assert_eq!(
        (t["state"].as_str(), t["app_id"].as_str()),
        (Some("canceled"), Some(APP))
    );
}

/// A receiver that stops reading keeps no file open: an end lets go of the
/// download's file at once, though its body is never read again, so the fd
/// closes and the deleted file's space goes (2026-10-22). Before, the body
/// held the file until its connection died.
#[tokio::test]
async fn an_end_lets_go_of_a_download_nobody_reads() {
    let (w, desktop, phone) = two().await;
    let bytes = file(64 << 20);
    let id = ready(&w, &desktop, &phone, &bytes).await;
    let path = path_of(&w, &id);
    let resp = phone
        .client
        .get(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // The body is never read: lmgw writes what the sockets take, then
    // waits.
    assert_eq!(w.state.transfers.downloads_open_for_tests(), 1);
    #[cfg(target_os = "linux")]
    assert_eq!(fds_on(&path), 1, "the download's");
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        &format!("/transfers/api/transfers/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (200, Some("canceled")), "{v}");
    assert_eq!(
        w.state.transfers.downloads_open_for_tests(),
        0,
        "the end let go of the file"
    );
    assert!(!path.exists());
    // Closed once a read in flight returned.
    #[cfg(target_os = "linux")]
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while fds_on(&path) > 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the deleted file stays open"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    drop(resp);
}

/// How many of this process's fds name `path` (`… (deleted)` once its name
/// went).
#[cfg(target_os = "linux")]
fn fds_on(path: &std::path::Path) -> usize {
    let dir = path.parent().unwrap().canonicalize().unwrap();
    let want = dir.join(path.file_name().unwrap());
    let want = want.to_str().unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|l| {
            l.to_str()
                .is_some_and(|l| l == want || l.strip_suffix(" (deleted)") == Some(want))
        })
        .count()
}
