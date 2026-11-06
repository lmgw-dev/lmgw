//! What becomes of a job its call let go running (design §5.4): the
//! route's timeout cancels it by the rule a stop takes, and an abandoned job
//! holds its claim while it still draws — followed by the job's own pace,
//! never by a clock that started with the call; a job that stops advancing
//! on a route with no timeout is given up and its row restarted; a native
//! cancel the server refuses falls back to the restart; and a caller that
//! cannot reach the thread cannot cancel its call.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use lmgw_core::runtime::Class;
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::chat_turn::{answer, chat_model, send, thread, tool_call, until};
use super::{call, gateway_recording, generating, image_row, job, text_of, CAPS, Z_IMAGE};

/// A job that draws one step a poll — or stays `queued` — up to `freeze`
/// steps, then reads the same step forever, until `done`.
struct Drawing {
    queued: bool,
    freeze: u32,
    polls: AtomicU32,
    done: AtomicBool,
}

impl Drawing {
    fn new(queued: bool, freeze: u32) -> Arc<Self> {
        Arc::new(Self {
            queued,
            freeze,
            polls: AtomicU32::new(0),
            done: AtomicBool::new(false),
        })
    }

    fn polls(&self) -> u32 {
        self.polls.load(Ordering::SeqCst)
    }
}

struct Poll(Arc<Drawing>);

impl Respond for Poll {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let n = self.0.polls.fetch_add(1, Ordering::SeqCst) + 1;
        let body = if self.0.done.load(Ordering::SeqCst) {
            job("completed")
        } else if self.0.queued {
            let mut q = job("queued");
            q["queue_position"] = json!(1);
            q
        } else {
            generating(1, n.min(self.0.freeze), "")
        };
        ResponseTemplate::new(200).set_body_json(body)
    }
}

/// sd-server on `sd` with its one job `d`; its native cancel answers
/// `cancel`.
async fn mount(sd: &MockServer, d: &Arc<Drawing>, cancel: u16) {
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CAPS.to_string()))
        .mount(sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/img_gen"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "id": "job_1", "kind": "img_gen", "status": "queued", "created": 1000,
            "poll_url": "/sdcpp/v1/jobs/job_1",
        })))
        .mount(sd)
        .await;
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/jobs/job_1"))
        .respond_with(Poll(d.clone()))
        .mount(sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/jobs/job_1/cancel"))
        .respond_with(ResponseTemplate::new(cancel).set_body_json(json!({"id": "job_1"})))
        .mount(sd)
        .await;
}

/// The image route's timeout, `secs` (0: none).
pub(super) async fn timeout_seconds(state: &SharedState, secs: u64) {
    let mut s = state.snapshot().settings.clone();
    s.image.request_timeout_seconds = secs;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

/// The image row's requests in flight.
pub(super) fn in_flight(state: &SharedState) -> u32 {
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == Class::Image && v.model_id == "z")
        .map(|v| v.in_flight)
        .unwrap_or(0)
}

fn draw() -> Value {
    json!({"model": "image/z", "prompt": "a red square"})
}

/// The route's timeout with another request on the row: the job is
/// abandoned, and followed holding its claim for as long as it still draws
/// — past the timeout that ended its call — until it ends.
#[tokio::test]
async fn a_timed_out_picture_holds_its_claim_while_its_job_still_draws() {
    let sd = MockServer::start().await;
    let d = Drawing::new(false, u32::MAX);
    mount(&sd, &d, 200).await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    timeout_seconds(&state, 2).await;
    let running = tokio::spawn({
        let gw = gw.clone();
        async move { call(&gw, "image__generate", draw()).await }
    });
    until("the job is polled", || async { d.polls() >= 2 }).await;
    let route = state.snapshot().resolve("image/z").unwrap();
    let other = lmgw_core::vram::admit(&state, &route, "image/z")
        .await
        .unwrap()
        .expect("a claim on the running row");

    let result = running.await.unwrap();
    let text = text_of(&result);
    assert_eq!(result["isError"], json!(true), "{result}");
    assert!(
        text.contains("image/z did not finish the picture within the route's timeout (2 s"),
        "{text}"
    );
    assert!(
        text.contains("the job runs to its end holding its claim"),
        "{text}"
    );
    // Still drawing: still followed, still claimed.
    let after = d.polls();
    until("the abandoned job is still followed", || async {
        d.polls() > after + 2
    })
    .await;
    assert_eq!(in_flight(&state), 2, "the abandoned job holds its claim");
    d.done.store(true, Ordering::SeqCst);
    until("its claim goes once it ended", || async {
        in_flight(&state) == 1
    })
    .await;
    drop(other);
    assert!(podman.stopped().is_empty(), "{:?}", podman.stopped());
}

/// The route's timeout on the row's only request: its container is
/// stopped, and the error says so.
#[tokio::test]
async fn a_timed_out_picture_on_its_own_row_restarts_it() {
    let sd = MockServer::start().await;
    let d = Drawing::new(false, u32::MAX);
    mount(&sd, &d, 200).await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    timeout_seconds(&state, 1).await;
    let result = call(&gw, "image__generate", draw()).await;
    let text = text_of(&result);
    assert!(text.contains("within the route's timeout (1 s"), "{text}");
    assert!(text.contains("lmgw stopped image/z's container"), "{text}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
}

/// No timeout on the route: a job that stops advancing is given up by its
/// own pace — twenty of its slowest steps, here about five seconds — and,
/// the row's only request, its container is stopped.
#[tokio::test]
async fn a_stalled_job_on_a_route_with_no_timeout_is_given_up() {
    let sd = MockServer::start().await;
    let d = Drawing::new(false, 3);
    mount(&sd, &d, 200).await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    timeout_seconds(&state, 0).await;
    let result = call(&gw, "image__generate", draw()).await;
    let text = text_of(&result);
    assert_eq!(result["isError"], json!(true), "{result}");
    assert!(
        text.contains("image/z's job made no progress for"),
        "{text}"
    );
    assert!(text.contains("the route sets no timeout"), "{text}");
    assert!(text.contains("lmgw stopped image/z's container"), "{text}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
    assert_eq!(in_flight(&state), 0);
}

/// A native cancel the server refuses (`409`: the job began generating
/// between the poll and the cancel) falls back to the rule without the
/// feature: here the row's only request, so its container is stopped.
#[tokio::test]
async fn a_refused_native_cancel_falls_back_to_the_restart() {
    let sd = MockServer::start().await;
    let d = Drawing::new(true, u32::MAX);
    mount(&sd, &d, 409).await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    timeout_seconds(&state, 1).await;
    let result = call(&gw, "image__generate", draw()).await;
    // Chosen at the drop, before the server refused.
    let text = text_of(&result);
    assert!(text.contains("the image server dropped the job"), "{text}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
    let posts = sd
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/sdcpp/v1/jobs/job_1/cancel")
        .count();
    assert_eq!(posts, 1);
}

/// A caller that cannot reach the thread — a device, the thread out of its
/// reach — is told the thread is not there, and the call runs on.
#[tokio::test]
async fn a_caller_that_cannot_reach_the_thread_cannot_cancel_its_call() {
    let sd = MockServer::start().await;
    let d = Drawing::new(false, u32::MAX);
    mount(&sd, &d, 200).await;
    let chat = MockServer::start().await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    chat_model(
        &state,
        &chat,
        &[
            tool_call("c1", "image__generate", draw()),
            answer("never mind then"),
        ],
    )
    .await;
    let tid = thread(&gw).await;
    // The self-admin toolset takes the thread out of a device's reach.
    let resp = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/settings"))
        .json(&json!({"mcp_tools": [{"server_label": "image"}, {"server_label": "lmgw"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let device = crate::support::device_pair::pair(&state, &gw, "phone", json!({})).await;
    let turn = tokio::spawn({
        let gw = gw.clone();
        async move { send(&gw, tid, "draw me a red square").await }
    });
    until("the job is polled", || async { d.polls() >= 2 }).await;

    let resp = device
        .client
        .post(format!("{gw}/chat/api/threads/{tid}/media/c1/cancel"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], json!("not_found"), "{body}");
    let after = d.polls();
    until("the call runs on", || async { d.polls() > after + 2 }).await;
    assert!(podman.stopped().is_empty());
    d.done.store(true, Ordering::SeqCst);
    let body = turn.await.unwrap();
    assert!(body.contains("never mind then"), "{body}");
}
