//! A stored file served to a player (`web::ranged`), through the lab
//! artifact route: `Accept-Ranges: bytes` on the whole file, one `Range` as
//! 206 with its `Content-Range` (the form WebKitGTK resumes a stopped
//! download with, `bytes=<n>-`, included), a range past the end as 416, the
//! forms lmgw does not read as the whole file, `If-Range` against the
//! artifact's `ETag`, and a HEAD ignoring `Range`. The file is larger than
//! one read block, so a span crossing blocks comes back byte for byte.

use reqwest::header::{HeaderMap, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG};
use reqwest::StatusCode;

use crate::common::{serve, Gw};
use crate::labs_store::{bytes, group};
use lmgw_core::state::AppState;

/// A WAV header and a counting pattern: 1 MiB + 17 bytes, four read blocks
/// and a tail.
fn wav() -> Vec<u8> {
    let n = 1024 * 1024 + 17;
    let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    b.extend((b.len()..n).map(|i| (i % 251) as u8));
    b
}

async fn artifact(body: &[u8]) -> (Gw, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (_, ids) = group(&state, "s", &["m"], vec![bytes("x.wav", body)]).await;
    let url = format!("{gw}/labs/api/artifacts/{}", ids[0]);
    (gw, url)
}

async fn fetch(
    gw: &Gw,
    url: &str,
    method: reqwest::Method,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = gw.client().request(method, url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.unwrap();
    let (status, h) = (resp.status(), resp.headers().clone());
    (status, h, resp.bytes().await.unwrap().to_vec())
}

fn h(headers: &HeaderMap, name: impl reqwest::header::AsHeaderName) -> String {
    headers
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn the_whole_file_says_it_takes_ranges() {
    let body = wav();
    let (gw, url) = artifact(&body).await;
    let (status, headers, got) = fetch(&gw, &url, reqwest::Method::GET, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(h(&headers, ACCEPT_RANGES), "bytes");
    assert_eq!(h(&headers, CONTENT_LENGTH), body.len().to_string());
    assert!(h(&headers, ETAG).starts_with("\"sha256:"), "{headers:?}");
    assert_eq!(h(&headers, "content-security-policy"), "sandbox");
    assert_eq!(h(&headers, "x-content-type-options"), "nosniff");
    assert!(headers.get(CONTENT_RANGE).is_none());
    assert_eq!(got, body);
}

#[tokio::test]
async fn one_range_is_206_with_its_bytes() {
    let body = wav();
    let size = body.len();
    let (gw, url) = artifact(&body).await;
    // WebKitGTK's resume after its queue filled, a span across read blocks,
    // a span past the end (cut at it), a suffix, Chromium's opening ask.
    let resume = 700_001;
    for (range, first, last) in [
        (format!("bytes={resume}-"), resume, size - 1),
        ("bytes=200000-600000".to_string(), 200_000, 600_000),
        (format!("bytes=10-{}", size + 50), 10, size - 1),
        ("bytes=-100".to_string(), size - 100, size - 1),
        ("bytes=0-".to_string(), 0, size - 1),
    ] {
        let (status, headers, got) =
            fetch(&gw, &url, reqwest::Method::GET, &[("range", &range)]).await;
        assert_eq!(status, 206, "{range}");
        assert_eq!(
            h(&headers, CONTENT_RANGE),
            format!("bytes {first}-{last}/{size}"),
            "{range}"
        );
        assert_eq!(h(&headers, ACCEPT_RANGES), "bytes", "{range}");
        assert_eq!(
            h(&headers, CONTENT_LENGTH),
            (last - first + 1).to_string(),
            "{range}"
        );
        assert_eq!(h(&headers, "content-type"), "audio/wav", "{range}");
        assert_eq!(got, body[first..=last], "{range}");
    }
}

#[tokio::test]
async fn a_range_past_the_end_is_416_naming_the_size() {
    let body = wav();
    let (gw, url) = artifact(&body).await;
    for range in [format!("bytes={}-", body.len()), "bytes=-0".to_string()] {
        let (status, headers, got) =
            fetch(&gw, &url, reqwest::Method::GET, &[("range", &range)]).await;
        assert_eq!(status, 416, "{range}");
        assert_eq!(
            h(&headers, CONTENT_RANGE),
            format!("bytes */{}", body.len())
        );
        let err: serde_json::Value = serde_json::from_slice(&got).unwrap();
        assert_eq!(err["code"], "range_not_satisfiable", "{err}");
    }
}

#[tokio::test]
async fn what_lmgw_does_not_read_is_the_whole_file() {
    let body = wav();
    let (gw, url) = artifact(&body).await;
    for range in ["bytes=0-9,20-29", "items=0-9", "bytes=9-3", "bytes=x-"] {
        let (status, headers, got) =
            fetch(&gw, &url, reqwest::Method::GET, &[("range", range)]).await;
        assert_eq!(status, 200, "{range}");
        assert!(headers.get(CONTENT_RANGE).is_none(), "{range}");
        assert_eq!(got.len(), body.len(), "{range}");
    }
    // A HEAD answers the whole file's head, whatever Range it carries.
    let (status, headers, got) = fetch(
        &gw,
        &url,
        reqwest::Method::HEAD,
        &[("range", "bytes=10-19")],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(h(&headers, CONTENT_LENGTH), body.len().to_string());
    assert_eq!(h(&headers, ACCEPT_RANGES), "bytes");
    assert!(got.is_empty());
}

#[tokio::test]
async fn if_range_applies_the_range_only_for_this_file() {
    let body = wav();
    let (gw, url) = artifact(&body).await;
    let (_, headers, _) = fetch(&gw, &url, reqwest::Method::HEAD, &[]).await;
    let etag = h(&headers, ETAG);
    let (status, _, got) = fetch(
        &gw,
        &url,
        reqwest::Method::GET,
        &[("range", "bytes=5-9"), ("if-range", &etag)],
    )
    .await;
    assert_eq!(status, 206);
    assert_eq!(got, body[5..=9]);
    for other in ["\"sha256:00\"", "Tue, 27 Oct 2026 12:00:00 GMT"] {
        let (status, _, got) = fetch(
            &gw,
            &url,
            reqwest::Method::GET,
            &[("range", "bytes=5-9"), ("if-range", other)],
        )
        .await;
        assert_eq!(status, 200, "{other}");
        assert_eq!(got, body, "{other}");
    }
}

/// The audio lab's reference-clip route over a models dir holding `body`
/// as `voices/c.wav`; the file's path is returned to replace it.
async fn reference(body: &[u8]) -> (Gw, String, std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.audio.models_dir = dir.path().display().to_string();
    lmgw_core::store::save_settings(&state.db, &settings)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state).await;
    let file = dir.path().join("voices").join("c.wav");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, body).unwrap();
    let url = format!("{gw}/audio-lab/api/refs/c.wav");
    (gw, url, file, dir)
}

#[tokio::test]
async fn a_reference_clip_is_ranged_sandboxed_and_has_a_validator_of_its_own() {
    let body = wav();
    let (gw, url, file, _dir) = reference(&body).await;
    let (status, headers, got) = fetch(&gw, &url, reqwest::Method::GET, &[]).await;
    assert_eq!((status.as_u16(), got.len()), (200, body.len()));
    let etag = h(&headers, ETAG);
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    let mut answers = vec![(status, headers)];
    for range in ["bytes=5-9", "bytes=-0"] {
        let (s, hd, _) = fetch(&gw, &url, reqwest::Method::GET, &[("range", range)]).await;
        answers.push((s, hd));
    }
    assert_eq!(
        answers.iter().map(|a| a.0.as_u16()).collect::<Vec<_>>(),
        [200, 206, 416]
    );
    for (status, headers) in answers {
        assert_eq!(h(&headers, "x-content-type-options"), "nosniff", "{status}");
        assert_eq!(
            h(&headers, "content-security-policy"),
            "sandbox",
            "{status}"
        );
    }

    // The same file: the range applies.
    let (status, _, got) = fetch(
        &gw,
        &url,
        reqwest::Method::GET,
        &[("range", "bytes=5-9"), ("if-range", &etag)],
    )
    .await;
    assert_eq!(status, 206);
    assert_eq!(got, body[5..=9]);

    // Replaced (temp + rename): the old validator gets the whole new file.
    let mut other = body.clone();
    other[7] ^= 0xff;
    let temp = file.with_extension("tmp");
    std::fs::write(&temp, &other).unwrap();
    std::fs::rename(&temp, &file).unwrap();
    let (status, headers, got) = fetch(
        &gw,
        &url,
        reqwest::Method::GET,
        &[("range", "bytes=5-9"), ("if-range", &etag)],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(got, other);
    assert_ne!(h(&headers, ETAG), etag);
}

#[tokio::test]
async fn a_range_on_an_empty_clip_serves_it_whole() {
    let (gw, url, _file, _dir) = reference(b"").await;
    for range in ["bytes=0-", "bytes=-500", "bytes=0-9"] {
        let (status, headers, got) =
            fetch(&gw, &url, reqwest::Method::GET, &[("range", range)]).await;
        assert_eq!(status, 200, "{range}");
        assert_eq!(h(&headers, CONTENT_LENGTH), "0", "{range}");
        assert!(got.is_empty());
    }
}

#[tokio::test]
async fn a_last_position_past_u64_clips_to_the_file_and_an_unreadable_if_range_serves_it_whole() {
    let body = wav();
    let (gw, url) = artifact(&body).await;
    let (status, headers, got) = fetch(
        &gw,
        &url,
        reqwest::Method::GET,
        &[("range", "bytes=10-99999999999999999999999")],
    )
    .await;
    assert_eq!(status, 206);
    assert_eq!(
        h(&headers, CONTENT_RANGE),
        format!("bytes 10-{}/{}", body.len() - 1, body.len())
    );
    assert_eq!(got, body[10..]);
    let (status, _, _) = fetch(
        &gw,
        &url,
        reqwest::Method::GET,
        &[("range", "bytes=99999999999999999999999-")],
    )
    .await;
    assert_eq!(status, 416);

    let resp = gw
        .client()
        .get(&url)
        .header("range", "bytes=5-9")
        .header(
            "if-range",
            reqwest::header::HeaderValue::from_bytes(b"\"\xff\"").unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().len(), body.len());
}

/// WP13: a lab artifact is stored content, so the 200, the 206 and the 416
/// all send nosniff and the sandbox policy (and the type stays the row's).
#[tokio::test]
async fn an_artifact_is_sandboxed_on_200_206_and_416() {
    use crate::support::stored::assert_sandboxed;
    let body = wav();
    let (gw, url) = artifact(&body).await;
    for (range, want) in [(None, 200), (Some("bytes=10-19"), 206)] {
        let hs: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
        let (status, headers, _) = fetch(&gw, &url, reqwest::Method::GET, &hs).await;
        assert_eq!(status, want);
        assert_sandboxed(&headers, &format!("{range:?}"));
        assert_eq!(h(&headers, "content-type"), "audio/wav");
    }
    let past = format!("bytes={}-", body.len());
    let (status, headers, _) = fetch(&gw, &url, reqwest::Method::GET, &[("range", &past)]).await;
    assert_eq!(status, 416);
    assert_sandboxed(&headers, "416");
    let (status, headers, _) = fetch(&gw, &url, reqwest::Method::HEAD, &[]).await;
    assert_eq!(status, 200);
    assert_sandboxed(&headers, "HEAD");
}
