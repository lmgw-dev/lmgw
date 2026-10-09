//! The RPM download: streamed to the caller's path, sha256-checked when the
//! manifest has a hash, and removed on a mismatch.

use lmgw_update::{download_rpm, Error, Feed, FeedAuth, RpmArtifact};
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BODY: &[u8] = b"not really an rpm, but bytes all the same";

async fn serve() -> (MockServer, Feed) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/app.rpm"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY.to_vec()))
        .mount(&server)
        .await;
    let feed = Feed {
        url: format!("{}/latest.json", server.uri()),
        auth: FeedAuth::None,
    };
    (server, feed)
}

fn artifact(server: &MockServer, sha256: &str) -> RpmArtifact {
    RpmArtifact {
        file: "app-9.9.9-1.x86_64.rpm".into(),
        url: format!("{}/app.rpm", server.uri()),
        sha256: sha256.into(),
    }
}

fn body_sha256() -> String {
    hex::encode(Sha256::digest(BODY))
}

#[tokio::test]
async fn a_matching_hash_in_either_case_keeps_the_file() {
    let (server, feed) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("update.rpm");
    let upper = body_sha256().to_uppercase();
    download_rpm(
        &reqwest::Client::new(),
        &feed,
        &artifact(&server, &upper),
        &dest,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), BODY);
}

#[tokio::test]
async fn no_hash_in_the_manifest_skips_the_check() {
    let (server, feed) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("update.rpm");
    download_rpm(
        &reqwest::Client::new(),
        &feed,
        &artifact(&server, ""),
        &dest,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), BODY);
}

#[tokio::test]
async fn a_hash_mismatch_removes_the_partial_file() {
    let (server, feed) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("update.rpm");
    let wrong = "00".repeat(32);
    let e = download_rpm(
        &reqwest::Client::new(),
        &feed,
        &artifact(&server, &wrong),
        &dest,
    )
    .await
    .unwrap_err();
    let Error::Sha256Mismatch { expected, got } = &e else {
        panic!("not a mismatch: {e:?}");
    };
    assert_eq!(expected, &wrong);
    assert_eq!(got, &body_sha256());
    assert_eq!(
        e.to_string(),
        format!("sha256 mismatch: expected {wrong}, got {got}")
    );
    assert!(!dest.exists(), "the partial file is removed");
}
