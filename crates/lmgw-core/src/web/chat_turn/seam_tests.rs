//! The turn seam from a caller that is not a page (chat-voice design §7.1):
//! frames into the caller's channel, and the caller's own stop
//! ([`Stopped::Interrupted`]).

use std::time::Duration;

use axum::body::{Body, Bytes};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::*;
use crate::config::{Protocol, SelfAdmin, UpstreamKind};
use crate::proxy::stop_pair;
use crate::state::AppState;
use crate::store::{self, NewAlias, NewUpstream};

mod spoken;

/// An OpenAI-shaped upstream that streams `chunks` (each a `delta.content`)
/// and then holds the stream open: a model still generating.
pub(super) async fn generating_upstream(chunks: &'static [&'static str]) -> String {
    let handler = move || async move {
        let head = futures::stream::iter(chunks.iter().map(|t| {
            let line = json!({"choices": [{"delta": {"content": t}}]});
            Ok::<_, std::io::Error>(Bytes::from(format!("data: {line}\n\n")))
        }));
        let body = futures::StreamExt::chain(head, futures::stream::pending());
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(body))
            .unwrap()
    };
    let app = axum::Router::new().route("/chat/completions", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// An OpenAI-shaped upstream that streams `Hel` and `lo`, then waits until
/// `release` is notified before it streams ` world` and ends: deltas that
/// arrive after the test raised its stop.
async fn released_upstream(release: std::sync::Arc<tokio::sync::Notify>) -> String {
    let handler = move || {
        let release = release.clone();
        async move {
            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
            tokio::spawn(async move {
                let chunk = |t: &str| {
                    let line = json!({"choices": [{"delta": {"content": t}}]});
                    Ok(Bytes::from(format!("data: {line}\n\n")))
                };
                for t in ["Hel", "lo"] {
                    let _ = tx.send(chunk(t)).await;
                }
                release.notified().await;
                let _ = tx.send(chunk(" world")).await;
                let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
            });
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(
                    tokio_stream::wrappers::ReceiverStream::new(rx),
                ))
                .unwrap()
        }
    };
    let app = axum::Router::new().route("/chat/completions", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A gateway whose alias `m` answers from `base`, and a thread of `kind` on
/// it whose user message `hi` waits for its reply: the thread and that
/// message's id.
pub(super) async fn world(base: &str, kind: &str) -> (SharedState, ChatThread, i64) {
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: base.to_string(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "m".into(),
            upstream_id: up,
            upstream_model_id: "tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.self_admin = SelfAdmin::ReadOnly;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let tid = store::create_chat_thread(&state.db, "m", kind)
        .await
        .unwrap();
    let uid = store::append_chat_message(&state.db, tid, "user", "hi", "", None, None, None)
        .await
        .unwrap();
    let thread = ChatRepo::of(tid)
        .thread(&state, tid)
        .await
        .unwrap()
        .unwrap();
    (state, thread, uid)
}

/// Start a fresh turn answering `uid` with the caller's stop `opts`.
pub(super) async fn start(
    state: &SharedState,
    thread: &ChatThread,
    uid: i64,
    opts: TurnOpts,
) -> mpsc::Receiver<TurnFrame> {
    let (tx, rx) = mpsc::channel(64);
    let mode = TurnMode::Fresh {
        user_message_id: Some(uid),
    };
    let repo = ChatRepo::of(thread.id);
    let caps = Caps::default();
    assert!(
        start_turn_into(state, repo, thread, mode, caps, tx, opts)
            .await
            .is_ok(),
        "the turn starts"
    );
    rx
}

/// The next frame, or `None` once the turn closed its channel.
pub(super) async fn next(rx: &mut mpsc::Receiver<TurnFrame>) -> Option<(String, Value)> {
    let f = tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("the turn says something within 20 s")?;
    Some((f.event.to_string(), serde_json::from_str(&f.data).unwrap()))
}

/// Every frame up to the channel's close.
pub(super) async fn rest(rx: &mut mpsc::Receiver<TurnFrame>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    while let Some(f) = next(rx).await {
        out.push(f);
    }
    out
}

/// Frames up to and including the first `event`.
pub(super) async fn until(rx: &mut mpsc::Receiver<TurnFrame>, event: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    loop {
        let f = next(rx)
            .await
            .unwrap_or_else(|| panic!("no '{event}': {out:?}"));
        let done = f.0 == event;
        out.push(f);
        if done {
            return out;
        }
    }
}

async fn replies(state: &SharedState, tid: i64) -> Vec<String> {
    store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.content)
        .collect()
}

#[tokio::test]
async fn a_caller_stop_mid_stream_saves_the_partial_and_still_sends_done() {
    let base = generating_upstream(&["Hel", "lo"]).await;
    let (state, thread, uid) = world(&base, "chat").await;
    let (stop, signal) = stop_pair();
    let mut rx = start(
        &state,
        &thread,
        uid,
        TurnOpts {
            stop: Some(signal),
            ..Default::default()
        },
    )
    .await;
    let head = until(&mut rx, "delta").await;
    assert_eq!(head[0], ("turn".into(), json!({"user_message_id": uid})));
    // Both chunks are out before the stream stalls.
    let second = next(&mut rx).await.unwrap();
    assert_eq!(second, ("delta".into(), json!({"text": "lo"})));
    stop.stop();
    let tail = rest(&mut rx).await;
    let (event, done) = tail.last().expect("a frame after the stop");
    assert_eq!(event, "done", "{tail:?}");
    assert_eq!(done["aborted"], true, "{done}");
    assert_eq!(done["saved"], true, "{done}");
    assert!(
        !tail.iter().any(|(e, _)| e == "error"),
        "the caller's own stop is no error: {tail:?}"
    );
    let id = done["message_id"].as_i64().unwrap();
    assert!(id > uid, "{done}");
    assert_eq!(replies(&state, thread.id).await, ["Hello"]);
}

#[tokio::test]
async fn a_caller_stop_before_the_stream_says_done_without_a_message_id() {
    let base = generating_upstream(&["never read"]).await;
    let (state, thread, uid) = world(&base, "chat").await;
    let (stop, signal) = stop_pair();
    stop.stop();
    let mut rx = start(
        &state,
        &thread,
        uid,
        TurnOpts {
            stop: Some(signal),
            ..Default::default()
        },
    )
    .await;
    let frames = rest(&mut rx).await;
    assert_eq!(
        frames,
        [
            ("turn".into(), json!({"user_message_id": uid})),
            ("done".into(), json!({"aborted": true})),
        ],
        "nothing saved, and said so by the missing message_id"
    );
    assert!(replies(&state, thread.id).await.is_empty());
}

/// A page's turn is unaffected by a caller stop it does not have: no stop,
/// no `Interrupted`.
#[tokio::test]
async fn without_a_caller_stop_the_turn_runs_to_its_end() {
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&mock)
        .await;
    let (state, thread, uid) = world(&mock.uri(), "chat").await;
    let mut rx = start(&state, &thread, uid, TurnOpts::default()).await;
    let frames = rest(&mut rx).await;
    let done = &frames.last().unwrap().1;
    assert_eq!(done["aborted"], false, "{frames:?}");
    assert_eq!(done["saved"], true);
    assert_eq!(replies(&state, thread.id).await, ["ok"]);
}

/// The same for a tool thread: its loop is cancelled, its partial text saved,
/// and the caller gets `done` — not a budget error, since nothing ran out.
#[tokio::test]
async fn a_caller_stop_mid_tool_thread_saves_the_partial_and_still_sends_done() {
    let base = generating_upstream(&["Hel", "lo"]).await;
    let (state, thread, uid) = world(&base, "admin").await;
    let (stop, signal) = stop_pair();
    let mut rx = start(
        &state,
        &thread,
        uid,
        TurnOpts {
            stop: Some(signal),
            ..Default::default()
        },
    )
    .await;
    until(&mut rx, "delta").await;
    assert_eq!(
        next(&mut rx).await.unwrap(),
        ("delta".into(), json!({"text": "lo"})),
        "the loop relays live"
    );
    stop.stop();
    let tail = rest(&mut rx).await;
    assert_eq!(tail.len(), 1, "{tail:?}");
    let (event, done) = &tail[0];
    assert_eq!(event, "done");
    assert_eq!(done["aborted"], true, "{done}");
    assert_eq!(done["saved"], true, "{done}");
    assert!(done["ttfb_ms"].is_number(), "{done}");
    assert_eq!(replies(&state, thread.id).await, ["Hello"]);
}

/// A tool thread hears the caller's stop at once: what the model streams
/// after it is neither relayed nor saved (WP2 review m1).
#[tokio::test]
async fn a_tool_thread_relays_and_keeps_nothing_after_the_caller_stop() {
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let base = released_upstream(release.clone()).await;
    let (state, thread, uid) = world(&base, "admin").await;
    let (stop, signal) = stop_pair();
    let mut rx = start(
        &state,
        &thread,
        uid,
        TurnOpts {
            stop: Some(signal),
            ..Default::default()
        },
    )
    .await;
    until(&mut rx, "delta").await;
    assert_eq!(
        next(&mut rx).await.unwrap(),
        ("delta".into(), json!({"text": "lo"}))
    );
    stop.stop();
    // The model goes on generating after the stop.
    release.notify_one();
    let tail = rest(&mut rx).await;
    assert_eq!(tail.len(), 1, "only `done` after the stop: {tail:?}");
    assert_eq!(tail[0].0, "done");
    assert_eq!(tail[0].1["aborted"], true);
    assert_eq!(replies(&state, thread.id).await, ["Hello"]);
}

/// A caller that hands in a channel with no room for the opening `turn`
/// frame is refused at once rather than left waiting on itself — before
/// the turn takes the thread, so the thread's live turn goes on (WP11
/// server review n6).
#[tokio::test]
async fn a_channel_with_no_room_is_refused_rather_than_waited_on() {
    let (state, thread, uid) = world("http://127.0.0.1:9", "chat").await;
    let live = state.chat_live.begin(thread.id).await;
    let (tx, _rx) = mpsc::channel(1);
    tx.try_send(TurnFrame::new("filler", "{}".into())).unwrap();
    let mode = TurnMode::Fresh {
        user_message_id: Some(uid),
    };
    let started = tokio::time::timeout(
        Duration::from_secs(5),
        start_turn_into(
            &state,
            ChatRepo::of(thread.id),
            &thread,
            mode,
            Caps::default(),
            tx,
            TurnOpts::default(),
        ),
    )
    .await
    .expect("refused, not waited on");
    let Err(refused) = started else {
        panic!("a full channel is refused");
    };
    assert_eq!(refused.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!live.is_superseded(), "the live turn was cancelled");
}
