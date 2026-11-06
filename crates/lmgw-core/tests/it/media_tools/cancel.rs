//! Cancelling a running picture (design §5.4, §6.5): the image server's own
//! cancel where the build has one for the job's state, the row's container
//! stopped when the call is its only request, the job followed to its end
//! holding the claim when another request uses the row — through the
//! per-call cancel route and through the turn's Stop alike.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use lmgw_core::runtime::Class;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::chat_turn::{answer, chat_model, rows_where, send, thread, tool_call, until};
use super::progress::{events, progress_frames};
use super::{gateway_recording, generating, image_row, job, Gw, CAPS, Z_IMAGE};

/// A job that stays `status` (`queued` at place 1, or generating) until it
/// is cancelled or `done` is set, counting its polls.
struct Job {
    status: &'static str,
    cancelled: AtomicBool,
    done: AtomicBool,
    polls: AtomicUsize,
}

impl Job {
    fn new(status: &'static str) -> Arc<Self> {
        Arc::new(Self {
            status,
            cancelled: AtomicBool::new(false),
            done: AtomicBool::new(false),
            polls: AtomicUsize::new(0),
        })
    }

    fn polls(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }
}

struct Poll(Arc<Job>);

impl Respond for Poll {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0.polls.fetch_add(1, Ordering::SeqCst);
        let body = if self.0.cancelled.load(Ordering::SeqCst) {
            job("cancelled")
        } else if self.0.done.load(Ordering::SeqCst) {
            job("completed")
        } else if self.0.status == "queued" {
            let mut q = job("queued");
            q["queue_position"] = json!(1);
            q
        } else {
            generating(1, 3, "")
        };
        ResponseTemplate::new(200).set_body_json(body)
    }
}

struct CancelPost(Arc<Job>);

impl Respond for CancelPost {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0.cancelled.store(true, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(json!({"id": "job_1", "status": "cancelled"}))
    }
}

/// sd-server on `sd` answering `caps`, its one job `j`.
async fn mount(sd: &MockServer, caps: &str, j: &Arc<Job>) {
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_string(caps.to_string()))
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
        .respond_with(Poll(j.clone()))
        .mount(sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/jobs/job_1/cancel"))
        .respond_with(CancelPost(j.clone()))
        .mount(sd)
        .await;
}

/// The cancel `POST`s sd-server got.
async fn cancel_posts(sd: &MockServer) -> usize {
    sd.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/sdcpp/v1/jobs/job_1/cancel")
        .count()
}

/// A thread whose chat model draws once and then answers, its turn
/// started: the thread and the running turn's body.
async fn drawing(
    state: &SharedState,
    gw: &Gw,
    chat: &MockServer,
) -> (i64, tokio::task::JoinHandle<String>) {
    chat_model(
        state,
        chat,
        &[
            tool_call(
                "c1",
                "image__generate",
                json!({"model": "image/z", "prompt": "a red square"}),
            ),
            answer("never mind then"),
        ],
    )
    .await;
    let tid = thread(gw).await;
    let turn = tokio::spawn({
        let gw = gw.clone();
        async move { send(&gw, tid, "draw me a red square").await }
    });
    (tid, turn)
}

/// `POST …/media/{call}/cancel`: status and body.
async fn cancel(gw: &Gw, tid: i64, call: &str) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/media/{call}/cancel"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The result frame's output of the turn's tool call.
fn result_text(body: &str) -> String {
    events(body, "tool")
        .into_iter()
        .find(|f| f["event"] == "result")
        .and_then(|f| f["output"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("no result frame: {body}"))
}

/// The image row's in-flight count.
fn in_flight(state: &SharedState) -> u32 {
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == Class::Image && v.model_id == "z")
        .map(|v| v.in_flight)
        .unwrap_or(0)
}

#[tokio::test]
async fn a_queued_job_is_cancelled_on_the_image_server() {
    let sd = MockServer::start().await;
    let j = Job::new("queued");
    mount(&sd, CAPS, &j).await;
    let chat = MockServer::start().await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (tid, turn) = drawing(&state, &gw, &chat).await;
    until("the job is polled", || async { j.polls() >= 2 }).await;

    let (status, answer) = cancel(&gw, tid, "c1").await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer, json!({"ok": true, "cancel": "native"}));
    let body = turn.await.unwrap();
    // The turn goes on: its model is told, and answers.
    assert!(body.contains("never mind then"), "{body}");
    let text = result_text(&body);
    assert!(
        text.contains("Cancelled by the user; the picture was not made (the image server dropped the job). Do not try again unless asked."),
        "{text}"
    );
    until("the cancel reached the image server", || async {
        cancel_posts(&sd).await == 1
    })
    .await;
    assert!(podman.stopped().is_empty(), "{:?}", podman.stopped());
    // Its rows are written, `canceled`.
    let image = rows_where(&state, (Some("image"), None), 1).await;
    assert_eq!(image, [(200, Some("canceled".to_string()))]);
    // Gone from the running calls once it ended.
    assert_eq!(cancel(&gw, tid, "c1").await.0, 404);
}

#[tokio::test]
async fn a_generating_job_on_a_build_that_can_stop_it_is_cancelled_and_followed() {
    let sd = MockServer::start().await;
    let j = Job::new("generating");
    let caps = CAPS.replace(
        "\"cancel_generating\": false",
        "\"cancel_generating\": true",
    );
    assert_ne!(caps, CAPS, "the fixture spells the feature");
    mount(&sd, &caps, &j).await;
    let chat = MockServer::start().await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (tid, turn) = drawing(&state, &gw, &chat).await;
    until("the job is polled", || async { j.polls() >= 2 }).await;

    let (_, answer) = cancel(&gw, tid, "c1").await;
    assert_eq!(answer["cancel"], json!("native"));
    let body = turn.await.unwrap();
    assert!(result_text(&body).contains("the image server dropped the job"));
    until("the cancel reached the image server", || async {
        cancel_posts(&sd).await == 1
    })
    .await;
    // Followed to `cancelled`, holding the claim until then.
    until("the claim goes once the job is cancelled", || async {
        in_flight(&state) == 0
    })
    .await;
    assert!(podman.stopped().is_empty());
}

#[tokio::test]
async fn a_running_picture_that_cannot_be_stopped_restarts_its_only_user_s_row() {
    let sd = MockServer::start().await;
    let j = Job::new("generating");
    mount(&sd, CAPS, &j).await;
    let chat = MockServer::start().await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (tid, turn) = drawing(&state, &gw, &chat).await;
    until("the job is polled", || async { j.polls() >= 2 }).await;

    let (_, answer) = cancel(&gw, tid, "c1").await;
    assert_eq!(answer, json!({"ok": true, "cancel": "restart"}));
    let body = turn.await.unwrap();
    let text = result_text(&body);
    assert!(text.contains("lmgw stopped its container"), "{text}");
    assert!(body.contains("never mind then"), "{body}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
    assert_eq!(cancel_posts(&sd).await, 0, "the build cannot stop it");
    let frames = progress_frames(&body);
    assert!(
        frames.iter().any(|f| f["cancel"] == "restart"),
        "the frames said it before: {frames:#?}"
    );
}

#[tokio::test]
async fn a_running_picture_beside_another_request_is_abandoned_holding_its_claim() {
    let sd = MockServer::start().await;
    let j = Job::new("generating");
    mount(&sd, CAPS, &j).await;
    let chat = MockServer::start().await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (tid, turn) = drawing(&state, &gw, &chat).await;
    until("the job is polled", || async { j.polls() >= 2 }).await;
    // Another request on the row.
    let route = state.snapshot().resolve("image/z").unwrap();
    let other = lmgw_core::vram::admit(&state, &route, "image/z")
        .await
        .unwrap()
        .expect("a claim on the running row");
    assert_eq!(in_flight(&state), 2);
    let polled = j.polls();
    until("a poll after the other request came", || async {
        j.polls() >= polled + 2
    })
    .await;

    let (_, answer) = cancel(&gw, tid, "c1").await;
    assert_eq!(answer, json!({"ok": true, "cancel": "abandon"}));
    let body = turn.await.unwrap();
    assert!(
        result_text(&body).contains("the picture finishes in the background and is thrown away"),
        "{body}"
    );
    // The job is still followed, and its claim held.
    let after = j.polls();
    until("the abandoned job is still followed", || async {
        j.polls() > after + 1
    })
    .await;
    assert_eq!(in_flight(&state), 2, "the abandoned job holds its claim");
    j.done.store(true, Ordering::SeqCst);
    until("its claim goes once it ended", || async {
        in_flight(&state) == 1
    })
    .await;
    drop(other);
    assert!(podman.stopped().is_empty(), "{:?}", podman.stopped());
    assert_eq!(cancel_posts(&sd).await, 0);
}

/// The turn's Stop — here a newer turn of the thread — takes the same path.
#[tokio::test]
async fn a_stopped_turn_cancels_its_running_picture_the_same_way() {
    let sd = MockServer::start().await;
    let j = Job::new("generating");
    mount(&sd, CAPS, &j).await;
    let chat = MockServer::start().await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (tid, turn) = drawing(&state, &gw, &chat).await;
    until("the job is polled", || async { j.polls() >= 2 }).await;
    let _ = send(&gw, tid, "never mind").await;
    let _ = turn.await.unwrap();
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
    let tool = rows_where(&state, (None, Some("chat-tool")), 1).await;
    assert_eq!(tool, [(200, Some("canceled".to_string()))]);
}

#[tokio::test]
async fn a_call_that_is_not_running_is_not_found() {
    let sd = MockServer::start().await;
    let (_state, gw, _podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let tid = thread(&gw).await;
    let (status, body) = cancel(&gw, tid, "c9").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], json!("not_running"), "{body}");
    let (status, _) = cancel(&gw, 999_999, "c9").await;
    assert_eq!(status, 404);
}

/// A card action's `act-<boot>-<n>` id from its stream as read so far.
fn action_id(text: &str) -> Option<String> {
    events(text, "action")
        .first()
        .and_then(|f| f["call_id"].as_str().map(str::to_string))
}

/// A card action on `id` with `body`, cancelled through the media route
/// once `ready` says so: the route's answer and the whole stream.
async fn cancel_action<F, Fut>(
    gw: &Gw,
    (tid, id): (i64, i64),
    body: Value,
    ready: F,
) -> (Value, String)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut resp = gw
        .client()
        .post(format!("{gw}/chat/api/attachments/{id}/media"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let mut text = String::new();
    let call = loop {
        let chunk = resp.chunk().await.unwrap().expect("the stream is open");
        text.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(id) = action_id(&text) {
            break id;
        }
    };
    until("the action runs", &ready).await;
    let (status, answer) = cancel(gw, tid, &call).await;
    assert_eq!(status, 200, "{answer}");
    while let Some(chunk) = resp.chunk().await.unwrap() {
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    (answer, text)
}

/// A card action's cancel (design §6.5) is the same route and the same
/// path: here a re-roll's running picture, its row's only request.
#[tokio::test]
async fn a_card_action_is_cancelled_through_the_same_route() {
    let sd = MockServer::start().await;
    let j = Job::new("generating");
    mount(&sd, CAPS, &j).await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (tid, _mid, id) =
        super::actions::picture(&state, Some(super::actions::made()), "tool").await;
    let (answer, text) = cancel_action(&gw, (tid, id), json!({"action": "reroll"}), || async {
        j.polls() >= 2
    })
    .await;
    assert_eq!(answer, json!({"ok": true, "cancel": "restart"}));
    let error = events(&text, "error");
    assert!(
        error[0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Cancelled by the user")),
        "{text}"
    );
    assert!(events(&text, "done").is_empty(), "{text}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
}

/// A synchronous upscale has no job: its cancel is the restart of its
/// row's only request (design §5.4).
#[tokio::test]
async fn a_running_upscale_is_cancelled_by_restarting_its_only_user_s_row() {
    let sd = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(super::upscale::caps_with_upscaler()),
        )
        .mount(&sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/upscale"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)))
        .mount(&sd)
        .await;
    let (state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    super::upscale::put_upscaler(&state);
    let (tid, _mid, id) =
        super::actions::picture(&state, Some(super::actions::made()), "tool").await;
    let upscaling = || async { !super::upscale::upscales(&sd).await.is_empty() };
    let (answer, text) =
        cancel_action(&gw, (tid, id), json!({"action": "upscale"}), upscaling).await;
    // No job, but its frames said what a cancel would do: the restart.
    assert_eq!(answer, json!({"ok": true, "cancel": "restart"}));
    let upscaling: Vec<Value> = events(&text, "progress")
        .into_iter()
        .filter(|f| f["phase"] == json!("upscaling"))
        .collect();
    assert!(
        !upscaling.is_empty() && upscaling.iter().all(|f| f["cancel"] == json!("restart")),
        "{text}"
    );
    let error = &events(&text, "error")[0];
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("lmgw stopped its container")),
        "{text}"
    );
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
}
