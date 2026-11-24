//! docling-serve's task routes over lmgw's jobs (document-conversion design
//! §7.4.1, D24): `GET /v1/status/poll/{task_id}` (with docling's `wait`),
//! `WS /v1/status/ws/{task_id}` (the same state, pushed on every change),
//! and `GET /v1/result/{task_id}` (the answer in the shape the request
//! asked for; docling's single-use results: the job goes
//! `RESULT_REMOVAL_DELAY` after its first fetch).
//!
//! A task another key made reads as not found, as docling-serve answers a
//! tenant's foreign task.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Json, Response};
use lmgw_api_types::documents::docling::{
    MessageKind, PollQuery, TaskStatusResponse, WebsocketMessage,
};
use lmgw_api_types::documents::jobs::JobState;

use super::{error, respond};
use crate::extract::convert::budget::answers;
use crate::extract::convert::docling_serve::answer::{self as docling, failure};
use crate::extract::convert::docling_serve::{Spec, Target};
use crate::extract::convert::jobs::answer::{self, Loaded};
use crate::extract::convert::jobs::sweep;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store;

fn not_found() -> Response {
    error(404, "Task not found.".into(), None)
}

/// Task `id`'s docling status, when `ctx` may see it.
pub async fn status_of(
    state: &SharedState,
    ctx: &RequestCtx,
    id: &str,
) -> Option<TaskStatusResponse> {
    answer::load(state, &ctx.principal, id)
        .await
        .map(|l| docling::task_status(&l))
}

/// A job's docling answer settings: the request's, or for a job of lmgw's
/// own route markdown, in the body for one file.
fn spec(l: &Loaded) -> Spec {
    l.row
        .adapter
        .as_deref()
        .and_then(|a| serde_json::from_str(a).ok())
        .unwrap_or(Spec {
            md: true,
            text: false,
            missing_outputs: Vec::new(),
            target: if l.files.len() == 1 {
                Target::Inbody
            } else {
                Target::Zip
            },
            abort_on_error: false,
            notes: None,
        })
}

/// Wait up to `wait` for job `id` to end.
async fn wait_for(state: &SharedState, id: &str, wait: Duration) {
    let Some(mut rx) = state.convert_jobs.watch(id) else {
        return;
    };
    let _ = tokio::time::timeout(wait, async {
        while rx.changed().await.is_ok() {
            if state.convert_jobs.watch(id).is_none() {
                break;
            }
        }
    })
    .await;
}

/// The longest one poll waits for its task to end (security round A): a
/// `wait` above it waits this long and the answer says so in
/// `x-lmgw-poll-wait`. Each waiting poll holds a connection; a client polls
/// again for longer.
pub const POLL_WAIT_MAX_S: u64 = 120;

/// `x-lmgw-poll-wait`.
pub const POLL_WAIT_HEADER: &str = "x-lmgw-poll-wait";

/// `wait` seconds as a duration, at most [`POLL_WAIT_MAX_S`], and whether
/// it was cut to it.
fn wait_of(seconds: f64) -> (Duration, bool) {
    let most = Duration::from_secs(POLL_WAIT_MAX_S);
    match Duration::try_from_secs_f64(seconds) {
        Ok(d) if d <= most => (d, false),
        _ => (most, true),
    }
}

/// Client messages one status WebSocket may send a second (security round
/// A): each reads the task's state again. A client that sends more has its
/// socket closed with 1008 and the bound's name.
pub const WS_CLIENT_MESSAGES_PER_S: u32 = 10;

/// `GET /v1/status/poll/{task_id}`.
pub async fn poll(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    Path(id): Path<String>,
    Query(q): Query<PollQuery>,
) -> Response {
    let mut cut = false;
    if q.wait > 0.0
        && q.wait.is_finite()
        && answer::load(&state, &ctx.principal, &id).await.is_some()
    {
        let (wait, was_cut) = wait_of(q.wait);
        cut = was_cut;
        wait_for(&state, &id, wait).await;
    }
    match status_of(&state, &ctx, &id).await {
        Some(s) => {
            let mut r = Json(s).into_response();
            if cut {
                if let Ok(v) = axum::http::HeaderValue::from_str(&format!(
                    "{POLL_WAIT_MAX_S} (POLL_WAIT_MAX_S)"
                )) {
                    r.headers_mut().insert(POLL_WAIT_HEADER, v);
                }
            }
            r
        }
        None => not_found(),
    }
}

/// `GET /v1/result/{task_id}`.
pub async fn result(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    Path(id): Path<String>,
    version: axum::http::Version,
    conn: Option<axum::Extension<axum::extract::ConnectInfo<crate::server::Conn>>>,
) -> Response {
    let Some(l) = answer::load(&state, &ctx.principal, &id).await else {
        return not_found();
    };
    match l.state() {
        JobState::Running => error(
            404,
            "Task result not found. Please wait for a completion status.".into(),
            None,
        ),
        JobState::Cancelled | JobState::Interrupted => {
            let message = l
                .row
                .error
                .clone()
                .unwrap_or_else(|| "the task did not complete".into());
            let retry = l.state() == JobState::Interrupted;
            Json(failure(message, retry)).into_response()
        }
        JobState::Succeeded | JobState::Failed => {
            // The stored answer is read into memory: it holds a share of the
            // answers in flight as a sync answer does.
            let a = answers::global();
            let who = super::native::caller(&ctx).share;
            let share = match a.hold(&who, a.output_budget()).await {
                Ok(s) => s,
                Err(e) => return error(503, e.message().into(), Some(answers::NeverFits::CODE)),
            };
            let s = spec(&l);
            let (_, body) = answer::full(&state, &l).await;
            let seconds = elapsed(&l);
            let (a, counted) = docling::render_off(&s, body.documents, seconds).await;
            // docling answers a task that failed with 200 and a
            // `TaskFailureResult`; only a task with no result is a 404. The
            // sync routes keep the 422 of a conversion that failed.
            let failed = matches!(a, docling::Answer::Failure(_));
            let to = super::native::send::To::on(conn.as_ref().map(|c| &c.0 .0), version);
            let (_, mut r) = respond(a, &counted, s.notes.as_deref(), share, to).await;
            if failed {
                *r.status_mut() = axum::http::StatusCode::OK;
            }
            // The single-use clock starts at the first fetch only.
            if matches!(
                store::convert_jobs::mark_fetched(&state.db, &id).await,
                Ok((_, true))
            ) {
                sweep::after_fetch(&state);
            }
            r
        }
    }
}

fn elapsed(l: &Loaded) -> f64 {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (
        parse(&l.row.created_at),
        l.row.finished_at.as_deref().and_then(parse),
    ) {
        (Some(a), Some(b)) => (b - a).num_milliseconds().max(0) as f64 / 1000.0,
        _ => 0.0,
    }
}

/// A normal close (1000), as docling-serve closes once the task completed.
async fn close(socket: &mut WebSocket) {
    let _ = socket
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1000,
            reason: "".into(),
        })))
        .await;
}

fn frame(kind: MessageKind, task: Option<TaskStatusResponse>, error: Option<&str>) -> Message {
    let m = WebsocketMessage {
        message: kind,
        task,
        error: error.map(String::from),
    };
    Message::Text(serde_json::to_string(&m).unwrap_or_default().into())
}

/// `WS /v1/status/ws/{task_id}`.
pub async fn ws(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    Path(id): Path<String>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let ws = match ws {
        Ok(ws) => ws,
        Err(why) => {
            return error(
                426,
                format!(
                    "/v1/status/ws/{{task_id}} is a WebSocket route: {}",
                    why.body_text()
                )
                .into(),
                Some("upgrade_required"),
            )
        }
    };
    ws.on_upgrade(move |socket| Box::pin(stream(state, ctx, id, socket)))
}

/// docling's order: `connection` with the state, then `update` with it,
/// then an `update` on every change (and on every message the client
/// sends), closed once the task has completed.
async fn stream(state: SharedState, ctx: RequestCtx, id: String, mut socket: WebSocket) {
    let Some(first) = status_of(&state, &ctx, &id).await else {
        let _ = socket
            .send(frame(MessageKind::Error, None, Some("Task not found.")))
            .await;
        close(&mut socket).await;
        return;
    };
    if socket
        .send(frame(MessageKind::Connection, Some(first), None))
        .await
        .is_err()
    {
        return;
    }
    let mut rx = state.convert_jobs.watch(&id);
    let mut rate = Rate::default();
    loop {
        let Some(now) = status_of(&state, &ctx, &id).await else {
            close(&mut socket).await;
            return;
        };
        let done = !matches!(
            now.task_status,
            lmgw_api_types::documents::docling::ConversionStatus::Pending
                | lmgw_api_types::documents::docling::ConversionStatus::Started
        );
        if socket
            .send(frame(MessageKind::Update, Some(now), None))
            .await
            .is_err()
        {
            return;
        }
        if done {
            close(&mut socket).await;
            return;
        }
        let changed = async {
            match rx.as_mut() {
                Some(r) => {
                    if r.changed().await.is_err() {
                        // The job ended: its sender went with it.
                        rx = None;
                    }
                }
                None => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        tokio::select! {
            () = changed => {}
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {
                    if !rate.allows(std::time::Instant::now()) {
                        let _ = socket
                            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: 1008,
                                reason: format!(
                                    "more than WS_CLIENT_MESSAGES_PER_S \
                                     ({WS_CLIENT_MESSAGES_PER_S}) messages a second"
                                )
                                .into(),
                            })))
                            .await;
                        return;
                    }
                }
            },
        }
    }
}

/// A socket's client messages in the current second.
#[derive(Default)]
struct Rate {
    since: Option<std::time::Instant>,
    count: u32,
}

impl Rate {
    /// Whether one more message at `now` stays within
    /// [`WS_CLIENT_MESSAGES_PER_S`].
    fn allows(&mut self, now: std::time::Instant) -> bool {
        match self.since {
            Some(t) if now.duration_since(t) < Duration::from_secs(1) => {}
            _ => {
                self.since = Some(now);
                self.count = 0;
            }
        }
        self.count += 1;
        self.count <= WS_CLIENT_MESSAGES_PER_S
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_wait_is_cut_to_its_bound_and_said() {
        assert_eq!(wait_of(5.0), (Duration::from_secs(5), false));
        assert_eq!(wait_of(120.0), (Duration::from_secs(120), false));
        assert_eq!(wait_of(1e9), (Duration::from_secs(POLL_WAIT_MAX_S), true));
        assert_eq!(wait_of(1e300), (Duration::from_secs(POLL_WAIT_MAX_S), true));
    }

    #[test]
    fn a_socket_may_send_ten_messages_a_second() {
        let mut r = Rate::default();
        let t = std::time::Instant::now();
        assert!((0..10).all(|_| r.allows(t)));
        assert!(!r.allows(t));
        // The next second starts again.
        assert!(r.allows(t + Duration::from_millis(1001)));
    }
}
