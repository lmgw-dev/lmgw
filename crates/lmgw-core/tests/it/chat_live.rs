//! One live turn per Chat thread (review R1 finding 1): a turn stopped or
//! replaced before its first token lets go of everything it was waiting on,
//! the upstream request included, and a reply is saved only onto the history
//! it answered — never after an edit, a newer turn, or over an edited row.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::store;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, get_json, openai_sse, post, sse_events};
use crate::common::Gw;

async fn thread(gw: &Gw) -> i64 {
    post(gw, "/chat/api/threads", json!({"model_alias": "m"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

async fn messages(gw: &Gw, tid: i64) -> Vec<(String, String)> {
    get_json(gw, &format!("/chat/api/threads/{tid}")).await["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                m["content"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(r, c)| (r.to_string(), c.to_string()))
        .collect()
}

/// A streamed answer `text` that takes `delay` to start — a model loading,
/// or a long prefill.
async fn mount_slow(mock: &MockServer, asked: &str, text: &str, delay: Duration) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(asked))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(openai_sse(text, 10, 2), "text/event-stream")
                .set_delay(delay),
        )
        .mount(mock)
        .await;
}

/// The finding's scenario: send, Stop while the model is still loading, edit
/// the message and "Save & send". The stopped turn must not come back with a
/// one-token reply to the old text: the thread ends `[user', reply']`.
#[tokio::test]
async fn stop_then_edit_leaves_only_the_new_answer() {
    let mock = MockServer::start().await;
    mount_slow(
        &mock,
        "old question",
        "a stale answer",
        Duration::from_millis(1500),
    )
    .await;
    mount_slow(
        &mock,
        "new question",
        "the fresh answer",
        Duration::from_millis(300),
    )
    .await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;

    let mut first = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "old question"}),
    )
    .await;
    let chunk = first.chunk().await.unwrap().expect("the turn event");
    let turn = sse_events(std::str::from_utf8(&chunk).unwrap());
    let uid = turn[0].1["user_message_id"].as_i64().unwrap();
    // Stop: the page drops the stream before the first token.
    drop(first);

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{uid}/edit"),
        json!({"content": "new question"}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert_eq!(
        sse_events(&body).last().unwrap().1["aborted"],
        false,
        "{body}"
    );
    // Long enough for the stopped turn's delayed answer to have arrived, had
    // it still been waiting for it.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(
        messages(&gw, tid).await,
        pairs(&[("user", "new question"), ("assistant", "the fresh answer")])
    );
}

/// A continue outliving an edit of the reply it continues: the edit stands,
/// and the rewrite stops the continue at once (it does not run on to a save
/// that would be refused).
#[tokio::test]
async fn a_continue_never_overwrites_an_edit_of_its_reply() {
    let (state, gw, tid, rid, cont, mock) = slow_continue().await;
    let _ = (&state, &mock);
    let edited = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{rid}/edit"),
        json!({"content": "edited by hand"}),
    )
    .await;
    assert_eq!(edited.status(), 200);

    let body = cont.await.unwrap();
    let events = sse_events(&body);
    assert!(
        events.iter().any(|(e, _)| e == "error"),
        "the stopped continue says so: {body}"
    );
    let done = events.last().unwrap().1.clone();
    assert_eq!(done["aborted"], true, "{body}");
    assert_eq!(
        messages(&gw, tid).await,
        pairs(&[("user", "q"), ("assistant", "edited by hand")])
    );
}

/// A save the thread refuses (the row was edited behind the live-turn
/// registry's back, so nothing cancelled the turn) is not shown as a saved
/// reply: `done` says `saved: false` with no row id, and an `error` event
/// comes first.
#[tokio::test]
async fn a_refused_save_is_reported_not_shown_as_saved() {
    let (state, gw, tid, rid, cont, mock) = slow_continue().await;
    let _ = &mock;
    let update = store::ChatMessageUpdate {
        content: "edited by hand".into(),
        ..Default::default()
    };
    assert!(store::update_chat_message(&state.db, tid, rid, &update)
        .await
        .unwrap());

    let body = cont.await.unwrap();
    let events = sse_events(&body);
    let err = events.iter().position(|(e, _)| e == "error").expect(&body);
    assert!(events[err].1["message"]
        .as_str()
        .unwrap()
        .contains("was not saved"));
    let done = events.last().unwrap();
    assert_eq!(done.0, "done");
    assert_eq!(done.1["saved"], false, "{body}");
    assert_eq!(done.1["message_id"], 0);
    assert_eq!(
        messages(&gw, tid).await,
        pairs(&[("user", "q"), ("assistant", "edited by hand")])
    );
}

/// A normal turn says it was saved, and by whom it was answered.
#[tokio::test]
async fn done_carries_saved_and_the_answering_model() {
    let mock = MockServer::start().await;
    mount_slow(&mock, "hi", "hello", Duration::from_millis(10)).await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await;
    let events = sse_events(&r.text().await.unwrap());
    let done = &events.last().unwrap().1;
    assert_eq!(done["saved"], true);
    assert_ne!(done["message_id"], 0);
    assert_eq!(done["model"], "m");
    assert!(done["answered_by"].is_null());
}

/// A thread `[q, "half an answer"]` on a llama-server route whose continue
/// takes a while, the continue running (200 ms in): the world, the thread,
/// the row, and the continue's body.
async fn slow_continue() -> (
    lmgw_core::state::SharedState,
    Gw,
    i64,
    i64,
    tokio::task::JoinHandle<String>,
    MockServer,
) {
    let mock = MockServer::start().await;
    mount_slow(
        &mock,
        "half an answer",
        " and more.",
        Duration::from_millis(800),
    )
    .await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::Openai).await;
    let tid = thread(&gw).await;
    store::append_chat_message(&state.db, tid, "user", "q", "", None, None, None)
        .await
        .unwrap();
    let rid = store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "half an answer",
        "",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let gw2 = gw.clone();
    let cont = tokio::spawn(async move {
        post(
            &gw2,
            &format!("/chat/api/threads/{tid}/continue"),
            json!({}),
        )
        .await
        .text()
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    (state, gw, tid, rid, cont, mock)
}

// ---------------------------------------------------------------------------
// A raw upstream that says when a request's connection went away
// ---------------------------------------------------------------------------

/// An OpenAI-shaped upstream that never answers the first request (a model
/// that is still loading) and says when that request's connection closes;
/// every later request gets [`openai_sse`] of `answer`.
struct HangingFirst {
    base: String,
    /// The first request arrived.
    arrived: Arc<Notify>,
    /// The first request's connection was closed by the gateway.
    dropped: Arc<Notify>,
}

async fn hanging_first(answer: &'static str) -> HangingFirst {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let arrived = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let (a, d) = (arrived.clone(), dropped.clone());
    tokio::spawn(async move {
        let mut n = 0;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            n += 1;
            let (a, d) = (a.clone(), d.clone());
            tokio::spawn(async move {
                read_request(&mut sock).await;
                if n == 1 {
                    a.notify_one();
                    // Hold it: no answer, until the gateway hangs up.
                    let mut buf = [0u8; 256];
                    while matches!(sock.read(&mut buf).await, Ok(k) if k > 0) {}
                    d.notify_one();
                } else {
                    let body = openai_sse(answer, 10, 2);
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                connection: close\r\n\r\n";
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
            });
        }
    });
    HangingFirst {
        base,
        arrived,
        dropped,
    }
}

/// Read one HTTP/1.1 request: headers, then `content-length` bytes of body.
async fn read_request(sock: &mut tokio::net::TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let k = sock.read(&mut chunk).await.unwrap_or(0);
        if k == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..k]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let k = sock.read(&mut chunk).await.unwrap_or(0);
        if k == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..k]);
    }
}

/// A gateway whose alias `m` is the [`HangingFirst`] upstream.
async fn hanging_gateway(up: &HangingFirst) -> (lmgw_core::state::SharedState, Gw) {
    // `gateway` wants a MockServer only for its uri; build the same rows on
    // the raw upstream's address instead.
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "raw-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: up.base.clone(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 60_000,
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
        &store::NewAlias {
            alias: "m".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = crate::common::serve(state.clone()).await;
    (state, gw)
}

const WAIT: Duration = Duration::from_secs(10);

/// Stop before the first token: the upstream request is dropped at once —
/// the model is not left prefilling for nobody — and nothing is saved.
#[tokio::test]
async fn stop_before_the_first_token_drops_the_upstream_request() {
    let up = hanging_first("never used").await;
    let (_state, gw) = hanging_gateway(&up).await;
    let tid = thread(&gw).await;

    let first = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hello"}),
    )
    .await;
    tokio::time::timeout(WAIT, up.arrived.notified())
        .await
        .expect("the request reached the upstream");
    drop(first);
    tokio::time::timeout(WAIT, up.dropped.notified())
        .await
        .expect("the stopped turn hung up on the upstream");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(messages(&gw, tid).await, pairs(&[("user", "hello")]));
}

/// A newer turn of the same thread (a second tab, a send after a Stop that
/// never reached the server) cancels the one still waiting: its upstream
/// request is dropped, its stream says why and ends, and only the newer
/// answer is saved.
#[tokio::test]
async fn a_new_send_cancels_the_turn_still_waiting() {
    let up = hanging_first("the second answer").await;
    let (_state, gw) = hanging_gateway(&up).await;
    let tid = thread(&gw).await;

    let first = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "one"}),
    )
    .await;
    let first_body = tokio::spawn(async move { first.text().await.unwrap() });
    tokio::time::timeout(WAIT, up.arrived.notified())
        .await
        .expect("the first request reached the upstream");

    let second = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "two"}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert_eq!(
        sse_events(&second).last().unwrap().1["aborted"],
        false,
        "{second}"
    );

    tokio::time::timeout(WAIT, up.dropped.notified())
        .await
        .expect("the replaced turn hung up on the upstream");
    let first_body = tokio::time::timeout(WAIT, first_body)
        .await
        .expect("the replaced turn's stream ended")
        .unwrap();
    let events = sse_events(&first_body);
    let error = events
        .iter()
        .find(|(e, _)| e == "error")
        .unwrap_or_else(|| panic!("no error event: {first_body}"));
    assert!(
        error.1["message"].as_str().unwrap().contains("newer turn"),
        "{first_body}"
    );
    assert_eq!(events.last().unwrap().1["aborted"], true, "{first_body}");
    assert_eq!(
        messages(&gw, tid).await,
        pairs(&[
            ("user", "one"),
            ("user", "two"),
            ("assistant", "the second answer")
        ])
    );
}
