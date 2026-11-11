//! An installer download (the RPM here): streamed to the caller's path, sha256-checked when the
//! manifest has a hash, and removed on a mismatch.

use lmgw_update::{download, Artifact, Error, Feed, FeedAuth};
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

fn artifact(server: &MockServer, sha256: &str) -> Artifact {
    Artifact {
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
    download(
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
    download(
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
    let e = download(
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

#[cfg(unix)]
#[tokio::test]
async fn the_file_is_created_0600_and_a_planted_symlink_is_not_followed() {
    use std::os::unix::fs::{symlink, MetadataExt};
    let (server, feed) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"precious").unwrap();
    let dest = dir.path().join("update.rpm");
    symlink(&victim, &dest).unwrap();
    download(
        &reqwest::Client::new(),
        &feed,
        &artifact(&server, ""),
        &dest,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    let meta = std::fs::symlink_metadata(&dest).unwrap();
    assert!(meta.is_file(), "the link was replaced by a regular file");
    assert_eq!(meta.mode() & 0o7777, 0o600);
    assert_eq!(std::fs::read(&dest).unwrap(), BODY);
}

/// Windows: a leftover at the path is replaced by a new file that is
/// private (lmgw_private's meaning: this user, SYSTEM and the Administrators
/// only, even where the directory lets others read).
#[cfg(windows)]
#[tokio::test]
async fn the_file_is_created_private_and_a_leftover_is_replaced() {
    let (server, feed) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new("icacls")
        .arg(dir.path())
        .args(["/grant", "*S-1-5-32-545:(OI)(CI)(R)"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let dest = dir.path().join("update.rpm");
    std::fs::write(&dest, b"leftover").unwrap();
    download(
        &reqwest::Client::new(),
        &feed,
        &artifact(&server, ""),
        &dest,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), BODY);
    lmgw_private::check_path(&dest, lmgw_private::Others::Nothing).unwrap();
}
