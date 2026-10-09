//! Each `FeedAuth` sends its own header, and only that one, on both requests:
//! the manifest and the RPM it names.

use lmgw_update::{check, download_rpm, Error, Feed, FeedAuth};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The `PRIVATE-TOKEN` and `Deploy-Token` values one request carried.
type AuthSeen = (Option<String>, Option<String>);

fn manifest_body(server: &MockServer) -> serde_json::Value {
    serde_json::json!({
        "version": "9.9.9",
        "rpm": { "file": "app-9.9.9-1.x86_64.rpm", "url": format!("{}/app.rpm", server.uri()) }
    })
}

fn auth_seen(r: &Request) -> AuthSeen {
    let get = |name: &str| r.headers.get(name).map(|v| v.to_str().unwrap().to_string());
    (get("PRIVATE-TOKEN"), get("Deploy-Token"))
}

/// A check and a download against a feed with `auth`: what each request
/// reached the server with.
async fn check_and_download(auth: FeedAuth) -> Vec<AuthSeen> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/latest.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(manifest_body(&server)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/app.rpm"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"rpm".to_vec()))
        .mount(&server)
        .await;
    let http = reqwest::Client::new();
    let feed = Feed {
        url: format!("{}/latest.json", server.uri()),
        auth,
    };
    let info = check(&http, &feed, "1.0.0")
        .await
        .unwrap()
        .expect("9.9.9 is newer");
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("update.rpm");
    download_rpm(&http, &feed, &info.manifest.rpm, &dest)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    requests.iter().map(auth_seen).collect()
}

#[tokio::test]
async fn a_private_token_is_sent_as_private_token() {
    let got = check_and_download(FeedAuth::PrivateToken("user-t".into())).await;
    assert_eq!(got, vec![(Some("user-t".into()), None); 2]);
}

#[tokio::test]
async fn a_deploy_token_is_sent_as_deploy_token() {
    let got = check_and_download(FeedAuth::DeployToken("deploy-t".into())).await;
    assert_eq!(got, vec![(None, Some("deploy-t".into())); 2]);
}

#[tokio::test]
async fn no_auth_sends_no_header() {
    let got = check_and_download(FeedAuth::None).await;
    assert_eq!(got, vec![(None, None); 2]);
}

#[tokio::test]
async fn an_up_to_date_build_gets_none_and_a_refusal_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/latest.json"))
        .and(header("PRIVATE-TOKEN", "t"))
        .respond_with(ResponseTemplate::new(200).set_body_json(manifest_body(&server)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/latest.json"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let http = reqwest::Client::new();
    let url = format!("{}/latest.json", server.uri());
    let feed = Feed {
        url: url.clone(),
        auth: FeedAuth::PrivateToken("t".into()),
    };
    assert!(check(&http, &feed, "9.9.9").await.unwrap().is_none());

    let anonymous = Feed {
        url,
        auth: FeedAuth::None,
    };
    let e = check(&http, &anonymous, "1.0.0").await.unwrap_err();
    assert!(matches!(e, Error::Http(_)), "{e:?}");
    assert!(e.to_string().contains("401"), "{e}");
}

#[test]
fn debug_never_shows_a_token() {
    let auth = FeedAuth::PrivateToken("secret".into());
    assert_eq!(format!("{auth:?}"), "PrivateToken(..)");
    let feed = Feed {
        url: "https://example/latest.json".into(),
        auth: FeedAuth::DeployToken("secret".into()),
    };
    assert!(!format!("{feed:?}").contains("secret"));
}

/// Mount a manifest on `feed_server` naming an RPM on `rpm_server`.
async fn rpm_elsewhere(feed_server: &MockServer, rpm_server: &MockServer) {
    let rpm_url = format!("{}/app.rpm", rpm_server.uri());
    Mock::given(method("GET"))
        .and(path("/latest.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "version": "9.9.9",
            "rpm": { "file": "app.rpm", "url": rpm_url }
        })))
        .mount(feed_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/app.rpm"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"rpm".to_vec()))
        .mount(rpm_server)
        .await;
}

async fn run_download(feed_server: &MockServer) {
    let http = reqwest::Client::new();
    let feed = Feed {
        url: format!("{}/latest.json", feed_server.uri()),
        auth: FeedAuth::PrivateToken("user-t".into()),
    };
    let info = check(&http, &feed, "1.0.0").await.unwrap().unwrap();
    let dir = tempfile::tempdir().unwrap();
    download_rpm(&http, &feed, &info.manifest.rpm, &dir.path().join("u.rpm"))
        .await
        .unwrap();
}

async fn seen_by(server: &MockServer) -> Vec<AuthSeen> {
    let requests = server.received_requests().await.unwrap();
    requests.iter().map(auth_seen).collect()
}

#[tokio::test]
async fn an_rpm_on_another_origin_gets_no_auth() {
    let feed_server = MockServer::start().await;
    let rpm_server = MockServer::start().await;
    rpm_elsewhere(&feed_server, &rpm_server).await;
    run_download(&feed_server).await;
    assert_eq!(
        seen_by(&feed_server).await,
        vec![(Some("user-t".into()), None)]
    );
    assert_eq!(seen_by(&rpm_server).await, vec![(None, None)]);
}

#[tokio::test]
async fn an_rpm_on_the_same_origin_keeps_the_auth() {
    let server = MockServer::start().await;
    rpm_elsewhere(&server, &server).await;
    run_download(&server).await;
    assert_eq!(
        seen_by(&server).await,
        vec![(Some("user-t".into()), None); 2]
    );
}

#[tokio::test]
async fn a_different_port_is_another_origin() {
    // Same host, two listeners: only the port differs.
    let feed_server = MockServer::start().await;
    let rpm_server = MockServer::start().await;
    assert_eq!(feed_server.address().ip(), rpm_server.address().ip());
    assert_ne!(feed_server.address().port(), rpm_server.address().port());
    rpm_elsewhere(&feed_server, &rpm_server).await;
    run_download(&feed_server).await;
    assert_eq!(seen_by(&rpm_server).await, vec![(None, None)]);
}
