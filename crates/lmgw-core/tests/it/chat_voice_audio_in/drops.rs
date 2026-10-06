//! A heard turn whose connection drops under the audio, end to end through
//! the bound responder (voice-audio-input design §3.5; decision D5, review
//! V3 and V4), on the plain path and in a tool thread:
//!
//! - **a llama-server** that drops it is remembered for the session, with a
//!   note — the drop is evidence about the audio (`refusal::crashed`, read
//!   from `SentAs::llama_server` on the route the turn was sent on);
//! - **any other API** that drops it answers the same turn from the
//!   transcript, once, and nothing is kept (`refusal::dropped`).
//!
//! The upstream is a socket that closes the connection, before any answer,
//! on a request that carries audio, and answers any other: wiremock cannot
//! drop a connection.

use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::realtime::heard_response_for_tests;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};

use super::tools::{register, stub, Calls};
use super::turns::{audio, hears};

/// What the dropping upstream answers a request without audio.
const SAID: &str = "Aus dem Transkript.";

/// Every request the upstream got: `POST audio` (dropped), `POST text`
/// (answered), or `GET <path>`.
type Asked = Arc<Mutex<Vec<String>>>;

/// The upstream (module doc): its `/v1` base and what it was asked.
async fn dropping_upstream() -> (String, Asked) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let asked = Asked::default();
    let log = asked.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let Some(request) = read_request(&mut sock).await else {
                    return;
                };
                let head = request.lines().next().unwrap_or_default().to_string();
                if head.starts_with("GET ") {
                    let path = head.split(' ').nth(1).unwrap_or_default();
                    log.lock().unwrap().push(format!("GET {path}"));
                    let _ = sock
                        .write_all(&response("404 Not Found", "text/plain", ""))
                        .await;
                    return;
                }
                if request.contains("input_audio") {
                    log.lock().unwrap().push("POST audio".into());
                    // No answer at all: the connection closes under it.
                    return;
                }
                log.lock().unwrap().push("POST text".into());
                let sse = format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({"choices": [{"delta": {"content": SAID}}]})
                );
                let _ = sock
                    .write_all(&response("200 OK", "text/event-stream", &sse))
                    .await;
            });
        }
    });
    (format!("http://{addr}/v1"), asked)
}

/// One whole HTTP/1.1 request (its head and its `content-length` body), as
/// text; `None` when the peer went first.
async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        let Some(end) = text.find("\r\n\r\n") else {
            continue;
        };
        let length = text[..end]
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        if buf.len() >= end + 4 + length {
            return Some(String::from_utf8_lossy(&buf).into_owned());
        }
    }
}

fn response(status: &str, kind: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: \
         close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A gateway with audio input on whose alias `m` takes audio (by its
/// override) on the dropping upstream, served as `protocol`/`up_kind`; a
/// thread on it — a tool thread with a stub MCP server when `tools` — with
/// a history and the heard turn's user row: the state, the thread, the
/// row, and what the upstream was asked.
async fn world(
    protocol: Protocol,
    up_kind: UpstreamKind,
    tools: bool,
) -> (SharedState, i64, i64, Asked) {
    let state = AppState::init_for_tests().await.unwrap();
    let (base, asked) = dropping_upstream().await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "dropper".into(),
            protocol,
            kind: up_kind,
            base_url: base,
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
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
            upstream_model_id: "model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(hears()),
        },
    )
    .await
    .unwrap();
    let mut s = state.snapshot().settings.clone();
    s.chat_voice_audio_input = "on".into();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let tid = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    if tools {
        register(&state, &stub(Calls::default()).await).await;
        sqlx::query("UPDATE chat_threads SET mcp_tools = ?1 WHERE id = ?2")
            .bind(json!([{"server_label": "stub"}]).to_string())
            .bind(tid)
            .execute(&state.db)
            .await
            .unwrap();
    }
    for (role, text) in [("user", "Wie spät ist es?"), ("assistant", "Keine Ahnung.")] {
        store::append_chat_message(&state.db, tid, role, text, "", None, None, None)
            .await
            .unwrap();
    }
    let row =
        store::append_chat_message(&state.db, tid, "user", "Und morgen?", "", None, None, None)
            .await
            .unwrap();
    (state, tid, row, asked)
}

fn posts(asked: &Asked) -> Vec<String> {
    asked
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a.starts_with("POST"))
        .cloned()
        .collect()
}

/// D5 on the path: a llama-server's drop under the audio is kept for the
/// session with a note — read from the route the turn went out on, in the
/// plain stream (`SentAs::on`) and in the tool loop (`Answering`).
#[tokio::test]
async fn a_llama_server_that_drops_the_audio_is_remembered_for_the_session() {
    for (kind, tools) in [("plain", false), ("tools", true)] {
        let (state, tid, row, asked) =
            world(Protocol::LlamaCpp, UpstreamKind::LlamaServer, tools).await;
        let out = heard_response_for_tests(&state, tid, vec![audio()], row).await;
        assert!(out.result.is_err(), "{kind}: the response's error: {out:?}");
        assert!(out.notes.is_empty(), "{kind}: not retried: {out:?}");
        assert_eq!(out.remembered.len(), 1, "{kind}: {out:?}");
        let (model, why, note) = &out.remembered[0];
        assert_eq!(model, "m", "{kind}");
        assert!(
            why.starts_with("its server failed on the audio this session"),
            "{kind}: {why}"
        );
        assert!(
            note.as_deref()
                .is_some_and(|n| n.contains("later turns of this session")),
            "{kind}: {note:?}"
        );
        assert_eq!(posts(&asked), ["POST audio"], "{kind}");
    }
}

/// Review V4: any other API's drop under the audio answers the same turn
/// from the transcript, once, and the session keeps nothing — its next
/// turn tries the audio again.
#[tokio::test]
async fn another_apis_drop_answers_the_turn_from_the_transcript_and_keeps_nothing() {
    for (kind, tools) in [("plain", false), ("tools", true)] {
        let (state, tid, row, asked) = world(Protocol::Openai, UpstreamKind::Generic, tools).await;
        let out = heard_response_for_tests(&state, tid, vec![audio()], row).await;
        assert_eq!(out.result, Ok(SAID.to_string()), "{kind}: {out:?}");
        assert_eq!(out.notes.len(), 1, "{kind}: {out:?}");
        assert!(
            out.notes[0].starts_with("the connection to the model dropped under the audio"),
            "{kind}: {:?}",
            out.notes
        );
        assert!(out.remembered.is_empty(), "{kind}: nothing kept: {out:?}");
        assert_eq!(posts(&asked), ["POST audio", "POST text"], "{kind}");
    }
}
