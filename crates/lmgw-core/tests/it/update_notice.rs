//! The server image's update notice (headless server design §6.3, ruling 13,
//! §12's "Update notice"): in container mode a newer version on the feed
//! gives the About line and the log line naming the load-and-restart path,
//! and nothing is downloaded; with `update_check_enabled` off no request is
//! made; the desktop's answers are as they were.

use lmgw_core::config::deployment::{Deployment, StartFacts};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::update::notice::{self, Checked};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, captured_log::capture_log};

/// A feed whose manifest names `version`, with an RPM beside it that a
/// download would fetch.
async fn feed(version: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/latest.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "version": version, "notes": "", "pub_date": "",
            "rpm": {"file": "lmgw.rpm", "url": format!("{}/lmgw.rpm", server.uri())}
        })))
        .mount(&server)
        .await;
    std::env::set_var(
        "LMGW_UPDATE_ENDPOINT",
        format!("{}/latest.json", server.uri()),
    );
    server
}

async fn container() -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_start_for_tests(StartFacts {
        deployment: Deployment::Container,
        ..Default::default()
    });
    state
}

async fn op_message(gw: &common::Gw, name: &str) -> String {
    let live: Value = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    live["message"].as_str().unwrap_or_default().to_string()
}

async fn settings_full(gw: &common::Gw) -> Value {
    gw.client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// A newer version on the feed: the notice, its line in the log and on
/// About, the explicit check answering the same line — and only the
/// manifest was asked for.
#[tokio::test]
async fn a_newer_version_gives_the_about_line_and_the_log_line() {
    let _env = common::process_env_lock().await;
    let server = feed("999.0.0").await;
    let state = container().await;
    let running = env!("CARGO_PKG_VERSION");
    let line = format!(
        "lmgw 999.0.0 is out; this server runs {running}. Updates come as a new image archive: \
         load it and restart the service."
    );
    assert_eq!(notice::sentence("999.0.0", running), line);

    let (log, guard) = capture_log();
    let Checked::Newer(n) = notice::check_once(&state).await else {
        panic!("a newer version")
    };
    drop(guard);
    assert_eq!(
        (n.latest.as_str(), n.running.as_str()),
        ("999.0.0", running)
    );
    assert_eq!(n.sentence, line);
    assert!(log.text().contains(&line), "{}", log.text());
    assert_eq!(state.update_notice(), Some(n));

    let gw = common::serve(state.clone()).await;
    let full = settings_full(&gw).await;
    assert_eq!(
        full["update_notice"]["sentence"], line,
        "{}",
        full["update_notice"]
    );
    assert_eq!(op_message(&gw, "update_check").await, line);

    let asked: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(!asked.is_empty());
    assert!(
        asked.iter().all(|p| p == "/latest.json"),
        "nothing downloaded: {asked:?}"
    );

    // The feed catches up: the notice goes.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/latest.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "version": running, "notes": "", "pub_date": "",
            "rpm": {"file": "lmgw.rpm", "url": format!("{}/lmgw.rpm", server.uri())}
        })))
        .mount(&server)
        .await;
    assert_eq!(notice::check_once(&state).await, Checked::UpToDate);
    assert_eq!(state.update_notice(), None);
}

/// `update_check_enabled` switched off: the save clears the notice, and the
/// background check while off asks nothing and leaves the notice alone; an
/// explicit check, the administrator's own request, still asks and keeps
/// what it found.
#[tokio::test]
async fn with_the_check_off_no_request_is_made() {
    let _env = common::process_env_lock().await;
    let server = feed("999.0.0").await;
    let state = container().await;
    assert!(matches!(
        notice::check_once(&state).await,
        Checked::Newer(_)
    ));
    let gw = common::serve(state.clone()).await;
    let saved = gw
        .client()
        .post(format!("{gw}/api/op/settings_set"))
        .json(&json!({"update_check_enabled": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 200, "{}", saved.text().await.unwrap());
    assert_eq!(state.update_notice(), None, "the save cleared it");
    assert_eq!(settings_full(&gw).await["update_notice"], Value::Null);

    server.reset().await;
    assert_eq!(notice::check_once(&state).await, Checked::Off);
    assert!(server.received_requests().await.unwrap().is_empty());

    // The explicit check while off: asked, and kept.
    let server = feed("999.0.0").await;
    let line = op_message(&gw, "update_check").await;
    assert!(line.starts_with("lmgw 999.0.0 is out"), "{line}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let kept = state.update_notice().expect("the explicit check's notice");
    // The background tick, still off, leaves it as it is.
    assert_eq!(notice::check_once(&state).await, Checked::Off);
    assert_eq!(state.update_notice(), Some(kept));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    // A save of something else while off leaves it too; switching on and
    // off again clears it.
    for body in [
        json!({"sampling_alias": ""}),
        json!({"update_check_enabled": true}),
    ] {
        let r = gw
            .client()
            .post(format!("{gw}/api/op/settings_set"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(state.update_notice().is_some(), "{body}");
    }
    let r = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&json!({"update_check_enabled": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert_eq!(state.update_notice(), None, "the full save cleared it too");
}

/// The desktop: the explicit check still points at the tray, and the
/// settings answer carries no notice.
#[tokio::test]
async fn the_desktop_keeps_its_update_answer() {
    let _env = common::process_env_lock().await;
    let _server = feed("999.0.0").await;
    let gw = common::serve(AppState::init_for_tests().await.unwrap()).await;
    assert_eq!(
        op_message(&gw, "update_check").await,
        common::desktop_update_answer("999.0.0")
    );
    assert_eq!(settings_full(&gw).await["update_notice"], Value::Null);
}
