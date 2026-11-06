//! A job lost mid-poll (design §4.1, review finding 6): sd-server keeps its
//! jobs in memory, so a container that died answers no poll, and one that
//! restarted answers 404 for the job lmgw submitted. Either way the job is
//! submitted again, once; a second loss is the call's error.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{call, gateway, gateway_on, image_row, job, mount_sd, submitted, text_of, Z_IMAGE};

/// An sd-server on a listener of its own: dropping it closes the port, as
/// a container that died does (a pooled wiremock would keep listening).
async fn container() -> MockServer {
    MockServer::builder()
        .listener(std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .start()
        .await
}

/// Whether `sd` was polled for the job.
async fn polled(sd: &MockServer) -> bool {
    sd.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .any(|r| r.url.path() == "/sdcpp/v1/jobs/job_1")
}

async fn until_polled(sd: &MockServer) {
    for _ in 0..500 {
        if polled(sd).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the job was never polled");
}

fn draw() -> serde_json::Value {
    json!({"model": "image/z", "prompt": "a red square"})
}

#[tokio::test]
async fn a_container_that_dies_mid_poll_gets_the_job_again_once() {
    let first = container().await;
    mount_sd(&first, &[job("generating")]).await;
    let second = container().await;
    mount_sd(&second, &[job("completed")]).await;
    let ports = vec![first.address().port(), second.address().port()];
    let (_state, gw) = gateway_on(ports, &[image_row("z", Z_IMAGE, false)]).await;

    let running = tokio::spawn({
        let gw = gw.clone();
        async move { call(&gw, "image__generate", draw()).await }
    });
    until_polled(&first).await;
    assert_eq!(submitted(&first).await.len(), 1);
    drop(first);
    let r = running.await.unwrap();
    assert_eq!(r["isError"], json!(false), "{r}");
    assert_eq!(r["content"][1]["type"], json!("image"));
    assert_eq!(submitted(&second).await.len(), 1, "submitted again, once");
}

#[tokio::test]
async fn a_second_death_is_the_call_s_error() {
    let first = container().await;
    mount_sd(&first, &[job("generating")]).await;
    let second = container().await;
    mount_sd(&second, &[job("generating")]).await;
    let ports = vec![first.address().port(), second.address().port()];
    let (_state, gw) = gateway_on(ports, &[image_row("z", Z_IMAGE, false)]).await;

    let running = tokio::spawn({
        let gw = gw.clone();
        async move { call(&gw, "image__generate", draw()).await }
    });
    until_polled(&first).await;
    drop(first);
    until_polled(&second).await;
    assert_eq!(submitted(&second).await.len(), 1);
    drop(second);
    let r = running.await.unwrap();
    assert_eq!(r["isError"], json!(true), "{r}");
}

/// A container that restarted under the job answers its poll with 404: the
/// job is submitted again on it, and its second run is the picture.
#[tokio::test]
async fn a_job_the_server_no_longer_knows_is_submitted_again_once() {
    let sd = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/jobs/job_1"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not found"})))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&sd)
        .await;
    mount_sd(&sd, &[job("completed")]).await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let r = call(&gw, "image__generate", draw()).await;
    assert_eq!(r["isError"], json!(false), "{r}");
    assert_eq!(submitted(&sd).await.len(), 2);
}

#[tokio::test]
async fn a_job_lost_twice_is_the_call_s_error() {
    let sd = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/jobs/job_1"))
        .respond_with(ResponseTemplate::new(410))
        .with_priority(1)
        .mount(&sd)
        .await;
    mount_sd(&sd, &[job("completed")]).await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let r = call(&gw, "image__generate", draw()).await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        text_of(&r).contains("lost the job again"),
        "{}",
        text_of(&r)
    );
    assert_eq!(submitted(&sd).await.len(), 2);
}
