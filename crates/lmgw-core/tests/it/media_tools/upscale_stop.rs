//! An upscale or a job its image server stops working on (design §5.4):
//! the route's timeout lets an upscale go by the rule a stop takes — its
//! row restarted when it is the only request, else the upscale followed to
//! its end holding its claim — and a server that accepts and then says
//! nothing (a status read unanswered within lmgw's control-read bound) is
//! given up the same way, with no pace and no route timeout needed.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::cancel_follow::{in_flight, timeout_seconds};
use super::chat_turn::until;
use super::upscale::{caps_with_upscaler, put_upscaler, upscales};
use super::{b64, call, gateway_recording, image_row, job, png, text_of, CAPS, Z_IMAGE};

/// An answer that comes at once until `hang` is raised, then never (as far
/// as a test waits).
struct Hangs {
    body: Value,
    hang: Arc<AtomicBool>,
    reads: Arc<AtomicU32>,
}

impl Respond for Hangs {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let r = ResponseTemplate::new(200).set_body_json(self.body.clone());
        if self.hang.load(Ordering::SeqCst) {
            r.set_delay(Duration::from_secs(120))
        } else {
            r
        }
    }
}

/// sd-server with an upscaler, its upscale answering after `delay`, its
/// capabilities hanging once the returned flag is raised.
async fn mount_upscale(sd: &MockServer, delay: Duration) -> Arc<AtomicBool> {
    let hang = Arc::new(AtomicBool::new(false));
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(Hangs {
            body: caps_with_upscaler(),
            hang: hang.clone(),
            reads: Default::default(),
        })
        .mount(sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/upscale"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "images": [{"index": 0, "b64_json": b64(&png())}],
                    "upscaler": super::upscale::UPSCALER, "scale": 4, "repeats": 1,
                    "width": 256, "height": 256, "output_format": "png",
                }))
                .set_delay(delay),
        )
        .mount(sd)
        .await;
    hang
}

fn picture() -> Value {
    json!({"image": format!("data:image/png;base64,{}", b64(&png()))})
}

/// The route's timeout on the row's only request: its container is stopped.
#[tokio::test]
async fn a_timed_out_upscale_on_its_own_row_restarts_it() {
    let sd = MockServer::start().await;
    mount_upscale(&sd, Duration::from_secs(60)).await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    put_upscaler(&state);
    timeout_seconds(&state, 1).await;
    let result = call(&gw, "image__upscale", picture()).await;
    let text = text_of(&result);
    assert_eq!(result["isError"], json!(true), "{result}");
    assert!(
        text.contains("image/z did not finish the upscale within the route's timeout (1 s"),
        "{text}"
    );
    assert!(text.contains("lmgw stopped image/z's container"), "{text}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
    assert_eq!(in_flight(&state), 0);
}

/// The route's timeout with another request on the row: the upscale runs
/// to its end on the image server, and its claim is held until it does.
#[tokio::test]
async fn a_timed_out_upscale_beside_another_request_holds_its_claim_to_its_end() {
    let sd = MockServer::start().await;
    mount_upscale(&sd, Duration::from_secs(4)).await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    put_upscaler(&state);
    timeout_seconds(&state, 2).await;
    let running = tokio::spawn({
        let gw = gw.clone();
        async move { call(&gw, "image__upscale", picture()).await }
    });
    until("the upscale is sent", || async {
        !upscales(&sd).await.is_empty()
    })
    .await;
    let route = state.snapshot().resolve("image/z").unwrap();
    let other = lmgw_core::vram::admit(&state, &route, "image/z")
        .await
        .unwrap()
        .expect("a claim on the running row");

    let result = running.await.unwrap();
    let text = text_of(&result);
    assert!(text.contains("within the route's timeout (2 s"), "{text}");
    assert!(
        text.contains("the upscale runs to its end holding its claim"),
        "{text}"
    );
    assert_eq!(in_flight(&state), 2, "the upscale still holds its claim");
    until("its claim goes once the upscale ended", || async {
        in_flight(&state) == 1
    })
    .await;
    drop(other);
    assert!(podman.stopped().is_empty(), "{:?}", podman.stopped());
}

/// No route timeout, and an image server that stops answering while an
/// upscale runs on it: given up after the status-read bound, its row (the
/// upscale its only request) restarted.
#[tokio::test]
async fn an_upscale_on_a_server_that_stops_answering_is_given_up() {
    let sd = MockServer::start().await;
    let hang = mount_upscale(&sd, Duration::from_secs(120)).await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    put_upscaler(&state);
    timeout_seconds(&state, 0).await;
    let running = tokio::spawn({
        let gw = gw.clone();
        async move { call(&gw, "image__upscale", picture()).await }
    });
    until("the upscale is sent", || async {
        !upscales(&sd).await.is_empty()
    })
    .await;
    hang.store(true, Ordering::SeqCst);
    let result = running.await.unwrap();
    let text = text_of(&result);
    assert!(
        text.contains("image/z's image server did not answer a status read within 5 s"),
        "{text}"
    );
    assert!(text.contains("lmgw stopped image/z's container"), "{text}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
    assert_eq!(in_flight(&state), 0);
}

/// No route timeout and no steps (no preview), so no pace: a job whose
/// polls stop being answered is given up after the status-read bound, and
/// its row restarted.
#[tokio::test]
async fn a_job_whose_polls_go_unanswered_is_given_up_without_a_pace() {
    let sd = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CAPS.to_string()))
        .mount(&sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/img_gen"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "id": "job_1", "kind": "img_gen", "status": "queued", "created": 1000,
        })))
        .mount(&sd)
        .await;
    let mut no_steps = job("generating");
    no_steps.as_object_mut().unwrap().remove("preview");
    let (hang, reads) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU32::new(0)),
    );
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/jobs/job_1"))
        .respond_with(Hangs {
            body: no_steps,
            hang: hang.clone(),
            reads: reads.clone(),
        })
        .mount(&sd)
        .await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    timeout_seconds(&state, 0).await;
    let running = tokio::spawn({
        let gw = gw.clone();
        async move {
            call(
                &gw,
                "image__generate",
                json!({"model": "image/z", "prompt": "a red square"}),
            )
            .await
        }
    });
    until("the job is polled", || async {
        reads.load(Ordering::SeqCst) >= 2
    })
    .await;
    hang.store(true, Ordering::SeqCst);
    let result = running.await.unwrap();
    let text = text_of(&result);
    assert!(
        text.contains("image/z's image server did not answer a status read within 5 s"),
        "{text}"
    );
    assert!(text.contains("while it ran the job"), "{text}");
    assert!(text.contains("lmgw stopped image/z's container"), "{text}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
    assert_eq!(in_flight(&state), 0);
}
