//! A heard turn's tools wait for its user row (voice-audio-input design
//! §3.4, WP3 review #1): the model answers the audio before the transcript
//! is in, and on noise it may call a tool. No tool runs before the journal
//! wrote the turn's row; a veto runs none.
//!
//! - **The self-admin plane**, through the turn seam
//!   (`web::spoken_turn_held_for_tests`): an Admin Chat turn whose model
//!   calls `lmgw__mcp_servers` waits at the call until the test settles the
//!   row — a veto, and the call's result says it was not run; a row
//!   written, and it runs; a failed transcription, and none runs while
//!   the reply goes on; a row the store refused, and none runs.
//! - **An MCP server's tool**, a whole bound session on `gpu_world`'s
//!   `gemma` with the realtime fakes and a stub server that records each
//!   call: a noise turn never calls it; a turn with words calls it only
//!   once its user row is stored.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use lmgw_core::config::McpTransport;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewMcpServer};
use lmgw_core::web::spoken_turn_held_for_tests;

use super::session::{hearing, session};
use super::turns::{audio, row_written, thread, world};
use crate::realtime_chat_thread::{eventually, of_type, say, try_next, until_type};
use crate::support::gpu_world::{ANSWER, GIB};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::Ws;

/// Frames until (and with) the first that `f` takes.
async fn frames_until(
    frames: &mut tokio::sync::mpsc::Receiver<(String, Value)>,
    f: impl Fn(&str, &Value) -> bool,
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(20), frames.recv())
            .await
            .expect("a frame in time")
            .expect("the turn goes on");
        let hit = f(&next.0, &next.1);
        out.push(next);
        if hit {
            return out;
        }
    }
}

/// The tool frames of `frames` with `event` (`ready`, `result`, …).
fn tool<'a>(frames: &'a [(String, Value)], event: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|(e, d)| e == "tool" && d["event"] == event)
        .map(|(_, d)| d)
        .collect()
}

#[tokio::test]
async fn a_self_admin_call_waits_for_the_row_and_a_veto_runs_none() {
    let g = world().await;
    g.world()
        .calls_tool
        .insert("gemma".into(), "lmgw__mcp_servers".into());
    let tid = thread(&g, "gemma", "admin").await;
    let mut held = spoken_turn_held_for_tests(&g.state, tid, vec![audio()])
        .await
        .expect("started");
    let before = frames_until(&mut held.frames, |e, d| {
        e == "tool" && d["event"] == "ready"
    })
    .await;
    assert!(tool(&before, "result").is_empty(), "{before:?}");
    // The model asked; the call waits for the row.
    let waited = tokio::time::timeout(Duration::from_millis(400), held.frames.recv()).await;
    assert!(waited.is_err(), "nothing more before the row: {waited:?}");
    held.veto();
    let rest = held.rest().await;
    let results = tool(&rest, "result");
    assert_eq!(results.len(), 1, "{rest:?}");
    assert_eq!(results[0]["is_error"], true);
    assert!(
        results[0]["output"]
            .as_str()
            .unwrap()
            .starts_with("not run:"),
        "the tool did not run: {rest:?}"
    );
    let done = &rest.last().unwrap().1;
    assert_eq!(
        (&done["saved"], &done["aborted"]),
        (&json!(false), &json!(true))
    );
    assert_eq!(g.world().streamed_bodies.len(), 1, "no second model call");
    let rows = store::list_chat_messages(&g.state.db, tid).await.unwrap();
    assert_eq!(rows.len(), 2, "the history only: {rows:?}");

    // Words: the row is written, and the call runs after it.
    let g = world().await;
    g.world()
        .calls_tool
        .insert("gemma".into(), "lmgw__mcp_servers".into());
    let tid = thread(&g, "gemma", "admin").await;
    let mut held = spoken_turn_held_for_tests(&g.state, tid, vec![audio()])
        .await
        .expect("started");
    frames_until(&mut held.frames, |e, d| {
        e == "tool" && d["event"] == "ready"
    })
    .await;
    let waited = tokio::time::timeout(Duration::from_millis(400), held.frames.recv()).await;
    assert!(waited.is_err(), "nothing more before the row: {waited:?}");
    let id = row_written(&g, tid, "Welche MCP-Server gibt es?").await;
    held.written(id);
    let rest = held.rest().await;
    let results = tool(&rest, "result");
    assert_eq!(results.len(), 1, "{rest:?}");
    assert_eq!(results[0]["is_error"], false, "{rest:?}");
    assert_eq!(rest.last().unwrap().1["saved"], true, "{rest:?}");
    let rows = store::list_chat_messages(&g.state.db, tid).await.unwrap();
    assert_eq!(rows.last().unwrap().content, ANSWER);
    assert!(rows[rows.len() - 2].id == id, "the reply follows the row");
}

/// Verification review: a failed transcription runs no tool — the reply
/// plays and the user hears it, but nobody confirmed the turn had words —
/// and the model's reply goes on and is saved. A row the store refused
/// runs none either, and says so in its own words.
#[tokio::test]
async fn a_failed_transcript_runs_no_tool_and_the_reply_goes_on() {
    let g = world().await;
    g.world()
        .calls_tool
        .insert("gemma".into(), "lmgw__mcp_servers".into());
    let tid = thread(&g, "gemma", "admin").await;
    let mut held = spoken_turn_held_for_tests(&g.state, tid, vec![audio()])
        .await
        .expect("started");
    frames_until(&mut held.frames, |e, d| {
        e == "tool" && d["event"] == "ready"
    })
    .await;
    held.failed();
    let rest = held.rest().await;
    let results = tool(&rest, "result");
    assert_eq!(results.len(), 1, "{rest:?}");
    assert_eq!(results[0]["is_error"], true);
    assert_eq!(
        results[0]["output"],
        "not run: the transcript failed, so lmgw could not confirm what was said",
        "{rest:?}"
    );
    let done = &rest.last().unwrap().1;
    assert_eq!(
        (&done["saved"], &done["aborted"]),
        (&json!(true), &json!(false)),
        "{rest:?}"
    );
    assert_eq!(g.world().streamed_bodies.len(), 2, "the model went on");
    let rows = store::list_chat_messages(&g.state.db, tid).await.unwrap();
    assert_eq!(rows.last().unwrap().content, ANSWER);

    // The store refused the row: none runs, and the result says why.
    let g = world().await;
    g.world()
        .calls_tool
        .insert("gemma".into(), "lmgw__mcp_servers".into());
    let tid = thread(&g, "gemma", "admin").await;
    let mut held = spoken_turn_held_for_tests(&g.state, tid, vec![audio()])
        .await
        .expect("started");
    frames_until(&mut held.frames, |e, d| {
        e == "tool" && d["event"] == "ready"
    })
    .await;
    held.unwritten();
    let rest = held.rest().await;
    let results = tool(&rest, "result");
    assert_eq!(results.len(), 1, "{rest:?}");
    assert_eq!(
        results[0]["output"],
        "not run: the spoken turn's user message could not be stored, so this reply is not kept",
        "{rest:?}"
    );
    assert_eq!(rest.last().unwrap().1["saved"], false, "{rest:?}");
    assert_eq!(g.world().streamed_bodies.len(), 1, "no second model call");
}

/// WP3 review #8: a reply whose user row the journal could not write (the
/// store refused it) is not saved, and says so.
#[tokio::test]
async fn a_reply_whose_row_could_not_be_written_is_not_saved() {
    let g = world().await;
    let tid = thread(&g, "gemma", "chat").await;
    let mut held = spoken_turn_held_for_tests(&g.state, tid, vec![audio()])
        .await
        .expect("started");
    frames_until(&mut held.frames, |e, _| e == "stop").await;
    held.unwritten();
    let rest = held.rest().await;
    let error = rest.iter().find(|(e, _)| e == "error").expect("said");
    assert_eq!(error.1["code"], "not_saved", "{rest:?}");
    assert_eq!(rest.last().unwrap().1["saved"], false, "{rest:?}");
    let rows = store::list_chat_messages(&g.state.db, tid).await.unwrap();
    assert_eq!(rows.len(), 2, "the history only: {rows:?}");
}

/// What the stub server saw: per `tools/call`, how many user rows the
/// thread held at that moment.
#[derive(Clone, Default)]
pub(super) struct Calls {
    pub(super) rows_at_call: Arc<Mutex<Vec<usize>>>,
    pub(super) thread: Arc<OnceLock<(SharedState, i64)>>,
}

impl Calls {
    pub(super) fn count(&self) -> usize {
        self.rows_at_call.lock().unwrap().len()
    }
}

/// A minimal MCP Streamable-HTTP server with one tool, `echo`, that
/// records each call in `calls`: its URL.
pub(super) async fn stub(calls: Calls) -> String {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let handler = move |body: String| {
        let calls = calls.clone();
        async move {
            let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let Some(id) = req.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let result = match req["method"].as_str().unwrap_or("") {
                "initialize" => json!({
                    "protocolVersion": req.pointer("/params/protocolVersion")
                        .cloned().unwrap_or(json!("2025-06-18")),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stub", "version": "0.1.0"},
                }),
                "tools/list" => json!({"tools": [{
                    "name": "echo",
                    "description": "echo the input",
                    "inputSchema": {"type": "object", "properties": {}},
                }]}),
                "tools/call" => {
                    let users = match calls.thread.get() {
                        Some((state, tid)) => store::list_chat_messages(&state.db, *tid)
                            .await
                            .unwrap()
                            .iter()
                            .filter(|m| m.role == "user")
                            .count(),
                        None => 0,
                    };
                    calls.rows_at_call.lock().unwrap().push(users);
                    json!({"content": [{"type": "text", "text": "echoed"}], "isError": false})
                }
                _ => json!({}),
            };
            (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("mcp-session-id", "stub-session"),
                ],
                json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
            )
                .into_response()
        }
    };
    let app = axum::Router::new().route("/mcp", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

/// The stub registered as MCP server `stub` (its tool `stub__echo`).
pub(super) async fn register(state: &SharedState, url: &str) {
    store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            name: "stub-server".into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: Some(url.into()),
            headers: vec![],
            tool_prefix: "stub".into(),
            timeout_ms: 5_000,
            autostart: false,
            idle_seconds: 300,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    state.mcp.reconcile(&state.snapshot()).await;
}

/// A bound session on a thread of `gemma` with the stub attached, whose
/// model calls `stub__echo` first: the stub's record, the thread, the
/// socket — and the card, kept alive.
async fn tool_session() -> (
    crate::support::gpu_world::Gpu,
    crate::realtime_chat_thread::World,
    Calls,
    i64,
    Ws,
) {
    let (g, w) = hearing("local", 24 * GIB, 30).await;
    let calls = Calls::default();
    let url = stub(calls.clone()).await;
    register(&g.state, &url).await;
    g.world()
        .calls_tool
        .insert("gemma".into(), "stub__echo".into());
    let (tid, ws) = session(&w).await;
    w.set(tid, json!({"mcp_tools": [{"server_label": "stub"}]}))
        .await;
    let _ = calls.thread.set((g.state.clone(), tid));
    (g, w, calls, tid, ws)
}

/// Every event that comes within a second of each other.
async fn quiet(ws: &mut Ws) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(ev) = try_next(ws, 1).await {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn a_noise_turn_never_calls_the_mcp_tool_its_model_asked_for() {
    let (g, w, calls, tid, mut ws) = tool_session().await;
    let release = Arc::new(Notify::new());
    w.asr.push(Asr::HeldText(release.clone(), ""));
    say(&mut ws).await;
    eventually(
        "the model to hear the turn and ask for the tool",
        || async { !g.world().streamed_bodies.is_empty() },
    )
    .await;
    quiet(&mut ws).await;
    assert_eq!(calls.count(), 0, "not before the transcript");
    release.notify_one();
    let events = until_type(&mut ws, "response.done").await;
    let done = &of_type(&events, "response.done")[0]["response"];
    assert_eq!(done["status_details"]["reason"], "no_words", "{events:?}");
    quiet(&mut ws).await;
    assert_eq!(calls.count(), 0, "a veto runs no tool");
    assert_eq!(g.world().streamed_bodies.len(), 1, "no second model call");
    assert!(w.messages(tid).await.is_empty(), "nothing is written");
}

#[tokio::test]
async fn a_turn_with_words_calls_the_mcp_tool_once_its_row_is_stored() {
    let (g, w, calls, tid, mut ws) = tool_session().await;
    let release = Arc::new(Notify::new());
    w.asr
        .push(Asr::HeldText(release.clone(), "Wie spät ist es?"));
    say(&mut ws).await;
    eventually(
        "the model to hear the turn and ask for the tool",
        || async { !g.world().streamed_bodies.is_empty() },
    )
    .await;
    quiet(&mut ws).await;
    assert_eq!(calls.count(), 0, "not before the transcript");
    release.notify_one();
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let done = &of_type(&events, "response.done")[0]["response"];
    assert_eq!(done["status"], "completed", "{events:?}");
    assert_eq!(
        *calls.rows_at_call.lock().unwrap(),
        [1],
        "called once, with the user row already stored"
    );
    let rows = w.messages(tid).await;
    let shape: Vec<(&str, &str)> = rows.iter().map(|r| (r.0.as_str(), r.1.as_str())).collect();
    assert_eq!(shape, [("user", "Wie spät ist es?"), ("assistant", ANSWER)]);
}
