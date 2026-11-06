//! An upstream that falls silent mid-stream ends its client's stream,
//! visibly (`proxy::silence`): the row's `timeout_ms` bounds the wait for the
//! headers and each silence after them, keep-alives do not break a silence
//! once the answer shows, and the client gets its protocol's mid-stream
//! error before the end.
//!
//! The live check of 2026-11-05: a streamed chat through an aggregator got
//! its headers and one delta ("pong"), then nothing but the aggregator's
//! keep-alive comments; the client's stream stayed open until the client
//! gave up 181 s later. The fake upstream here does the same, with a
//! keep-alive every 50 ms, on a row whose timeout is [`TIMEOUT_MS`].
//!
//! One case per streamed relay of an `openai_compatible` row: the chat
//! relay as `/v1/chat/completions` and `/v1/messages` answer it, the
//! Responses loop, and the legacy completions relay. The native
//! `/v1/responses` passthrough is `responses_native::silence`. A
//! steady upstream whose gaps stay under the timeout is not cut, and a
//! timeout of 0 cuts nothing. Where keep-alives still count (a model
//! thinking out of sight) is [`keep_alive`].
//!
//! Cut cases run at [`TIMEOUT_MS`]; the ones that must not be cut at
//! [`LONG_TIMEOUT_MS`], with gaps of at most a quarter of it ([`GAP`]), and
//! their relays side by side, so a loaded box neither cuts them nor makes
//! them slow.

mod keep_alive;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};

use crate::common::{self, patience, Gw};

/// The upstream row's `timeout_ms` where a test waits for a cut.
const TIMEOUT_MS: u64 = 300;

/// The upstream row's `timeout_ms` where nothing may be cut.
const LONG_TIMEOUT_MS: u64 = 1_000;

/// The gap between an uncut upstream's frames.
const GAP: Duration = Duration::from_millis(250);

/// The frames an uncut upstream spreads [`GAP`] apart: more than three
/// [`LONG_TIMEOUT_MS`] in all.
const TRICKLES: usize = 14;

/// How long a cut stream may take past the timeout before the test calls it
/// uncut: lmgw's cut is a matter of milliseconds, this is a loaded box's
/// slack.
const SLACK: Duration = Duration::from_secs(3);

const ALIAS: &str = "silent-model";

/// How the fake upstream streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Says {
    /// Its head and one delta (`pong`), then only a keep-alive (a comment;
    /// Anthropic's `ping`) every 50 ms until its connection closes.
    Stalls,
    /// Its head and one delta, then [`TRICKLES`] deltas, each [`GAP`] after
    /// the last, then its finish, usage and `[DONE]`.
    Steady,
    /// Its head with no content yet, then [`TRICKLES`] keep-alive comments
    /// [`GAP`] apart, then `pong` and its finish: a model thinking out of
    /// sight behind an aggregator.
    ThinksFirst,
    /// Anthropic only: a text block (`pong`), then a thinking block whose
    /// only events are [`TRICKLES`] `ping`s [`GAP`] apart before its
    /// signature, then a text block (`done`) and the message's end:
    /// `thinking.display` omitted.
    ThinksHidden,
    /// Anthropic only: a text block (`pong`), then a `server_tool_use`
    /// block (a web search) and [`TRICKLES`] `ping`s [`GAP`] apart while
    /// the search runs, then its `web_search_tool_result` block, a text
    /// block (`done`) and the message's end.
    SearchesHidden,
    /// Anthropic only: [`Says::SearchesHidden`] up to the result block, its
    /// search quick, then only a `ping` every 50 ms until the connection
    /// closes: the tool's span is over, and so is the wait.
    SearchedThenStalls,
    /// Its head, one delta and its finish, then the connection held open.
    FinishesThenHolds,
    /// Its head, one delta and only the finish's first frame (a chat
    /// `finish_reason` with no usage chunk or `[DONE]` after it; Anthropic's
    /// `message_delta` with no `message_stop`), then the connection held
    /// open while a keep-alive comes every 50 ms.
    StopsThenHolds,
    /// As [`Says::StopsThenHolds`] up to the finish's first frame, then a
    /// broken body.
    StopsThenBreaks,
    /// Its head and one delta, then half an event, then nothing.
    HalfThenHolds,
    /// Its head and one delta, then half an event, then a broken body.
    HalfThenBreaks,
    /// [`Says::Steady`] in CRLF lines, each frame split in two reads
    /// [`GAP`] apart (every other one between a CR and its LF).
    SplitCrlf,
    /// Its head and one delta in CRLF lines, then a CRLF keep-alive comment
    /// split in two reads, every 50 ms.
    CrlfStalls,
}

/// The wire a fake route speaks.
#[derive(Clone, Copy)]
enum Wire {
    Chat,
    Legacy,
    Anthropic,
}

/// One step of a fake stream.
enum Step {
    Send(String),
    Wait(Duration),
    /// The body breaks.
    Break,
    /// Nothing more until the connection closes.
    Hold,
    /// These, one every given gap, round and round until the connection
    /// closes.
    Repeat(Vec<String>, Duration),
}

struct Upstream {
    says: Says,
    /// Streams whose connection closed under them.
    closed: AtomicUsize,
}

/// Counts a stream as closed once its producer stopped.
struct Open(Arc<Upstream>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.closed.fetch_add(1, Ordering::SeqCst);
    }
}

async fn upstream(says: Says) -> (String, Arc<Upstream>) {
    let seen = Arc::new(Upstream {
        says,
        closed: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .route("/completions", post(legacy))
        .route("/v1/messages", post(messages))
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

fn chat_chunk(delta: Value, finish: Option<&str>) -> String {
    let c = json!({"id": "chatcmpl-up", "object": "chat.completion.chunk", "created": 1,
                   "model": "tgt-model",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    format!("data: {c}\n\n")
}

fn legacy_chunk(text: &str, finish: Option<&str>) -> String {
    let c = json!({"id": "cmpl-up", "object": "text_completion", "created": 1,
                   "model": "tgt-model",
                   "choices": [{"index": 0, "text": text, "finish_reason": finish}]});
    format!("data: {c}\n\n")
}

fn anthropic_event(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

const USAGE: &str = "data: {\"id\":\"chatcmpl-up\",\"object\":\"chat.completion.chunk\",\
                     \"created\":1,\"model\":\"tgt-model\",\"choices\":[],\
                     \"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\n\n";

const KEEP_ALIVE: &str = ": PROCESSING\n\n";

/// The first half of an event, sent by [`Says::HalfThenHolds`] and
/// [`Says::HalfThenBreaks`].
const HALF_EVENT: &str = "data: {\"id\":\"cmpl-up\",\"choi";

impl Wire {
    /// The stream's head, before any content.
    fn opening(self) -> Vec<String> {
        match self {
            Wire::Chat => vec![chat_chunk(
                json!({"role": "assistant", "content": ""}),
                None,
            )],
            Wire::Legacy => vec![legacy_chunk("", None)],
            Wire::Anthropic => vec![
                anthropic_event(
                    "message_start",
                    json!({"type": "message_start", "message": {
                        "id": "msg_up", "type": "message", "role": "assistant",
                        "model": "tgt-model", "content": [], "stop_reason": null,
                        "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 1}}}),
                ),
                block_start(0, json!({"type": "text", "text": ""})),
            ],
        }
    }

    fn content(self, text: &str) -> String {
        match self {
            Wire::Chat => chat_chunk(json!({"content": text}), None),
            Wire::Legacy => legacy_chunk(text, None),
            Wire::Anthropic => text_delta(0, text),
        }
    }

    /// The answer's end.
    fn finish(self) -> Vec<String> {
        match self {
            Wire::Chat => vec![
                chat_chunk(json!({}), Some("stop")),
                USAGE.to_string(),
                "data: [DONE]\n\n".into(),
            ],
            Wire::Legacy => vec![legacy_chunk("", Some("stop")), "data: [DONE]\n\n".into()],
            Wire::Anthropic => vec![
                anthropic_event(
                    "message_delta",
                    json!({"type": "message_delta",
                           "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                           "usage": {"output_tokens": 6}}),
                ),
                anthropic_event("message_stop", json!({"type": "message_stop"})),
            ],
        }
    }

    fn keep_alive(self) -> String {
        match self {
            Wire::Anthropic => anthropic_event("ping", json!({"type": "ping"})),
            _ => KEEP_ALIVE.into(),
        }
    }

    /// What `says` streams on this wire.
    fn script(self, says: Says) -> Vec<Step> {
        let mut head: Vec<String> = self.opening();
        head.push(self.content("pong"));
        let sends = |frames: Vec<String>| frames.into_iter().map(Step::Send);
        match says {
            Says::Stalls => sends(head)
                .chain([Step::Repeat(
                    vec![self.keep_alive()],
                    Duration::from_millis(50),
                )])
                .collect(),
            Says::Steady => sends(head)
                .chain((0..TRICKLES).flat_map(|_| [Step::Wait(GAP), Step::Send(self.content("a"))]))
                .chain([Step::Wait(GAP)])
                .chain(sends(self.finish()))
                .collect(),
            Says::ThinksFirst => sends(self.opening())
                .chain((0..TRICKLES).flat_map(|_| [Step::Wait(GAP), Step::Send(self.keep_alive())]))
                .chain(sends(vec![self.content("pong")]))
                .chain(sends(self.finish()))
                .collect(),
            Says::ThinksHidden => {
                let thinking =
                    (0..TRICKLES).flat_map(|_| [Step::Wait(GAP), Step::Send(self.keep_alive())]);
                let after = vec![
                    block_delta(1, json!({"type": "signature_delta", "signature": "c2ln"})),
                    block_stop(1),
                    block_start(2, json!({"type": "text", "text": ""})),
                    text_delta(2, "done"),
                    block_stop(2),
                ];
                let before = vec![
                    block_stop(0),
                    block_start(1, json!({"type": "thinking", "thinking": ""})),
                ];
                sends(head)
                    .chain(sends(before))
                    .chain(thinking)
                    .chain(sends(after))
                    .chain(sends(self.finish()))
                    .collect()
            }
            Says::SearchesHidden | Says::SearchedThenStalls => {
                let tool = vec![
                    block_stop(0),
                    block_start(
                        1,
                        json!({"type": "server_tool_use", "id": "srvtoolu_1",
                               "name": "web_search", "input": {"query": "pong"}}),
                    ),
                    block_stop(1),
                ];
                let result = vec![
                    block_start(
                        2,
                        json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                               "content": []}),
                    ),
                    block_stop(2),
                ];
                if says == Says::SearchedThenStalls {
                    return sends(head)
                        .chain(sends(tool))
                        .chain(sends(result))
                        .chain([Step::Repeat(
                            vec![self.keep_alive()],
                            Duration::from_millis(50),
                        )])
                        .collect();
                }
                let searching =
                    (0..TRICKLES).flat_map(|_| [Step::Wait(GAP), Step::Send(self.keep_alive())]);
                let after = vec![
                    block_start(3, json!({"type": "text", "text": ""})),
                    text_delta(3, "done"),
                    block_stop(3),
                ];
                sends(head)
                    .chain(sends(tool))
                    .chain(searching)
                    .chain(sends(result))
                    .chain(sends(after))
                    .chain(sends(self.finish()))
                    .collect()
            }
            Says::FinishesThenHolds => sends(head)
                .chain(sends(self.finish()))
                .chain([Step::Hold])
                .collect(),
            Says::StopsThenHolds => sends(head)
                .chain(sends(self.finish().into_iter().take(1).collect()))
                .chain([Step::Repeat(
                    vec![self.keep_alive()],
                    Duration::from_millis(50),
                )])
                .collect(),
            Says::StopsThenBreaks => sends(head)
                .chain(sends(self.finish().into_iter().take(1).collect()))
                // Long enough for the frame to have left: an error ends the
                // body at once, buffered bytes with it.
                .chain([Step::Wait(Duration::from_millis(200)), Step::Break])
                .collect(),
            Says::HalfThenHolds => sends(head)
                .chain([Step::Send(HALF_EVENT.into()), Step::Hold])
                .collect(),
            Says::HalfThenBreaks => sends(head)
                .chain([
                    Step::Send(HALF_EVENT.into()),
                    // Long enough for the half to have left: an error ends
                    // the body at once, buffered bytes with it.
                    Step::Wait(Duration::from_millis(200)),
                    Step::Break,
                ])
                .collect(),
            Says::SplitCrlf => {
                let frames = head
                    .into_iter()
                    .chain((0..TRICKLES).map(|_| self.content("a")))
                    .chain(self.finish())
                    .map(|f| crlf(&f));
                frames
                    .enumerate()
                    .flat_map(|(i, f)| {
                        // Mid-line, or between the CR and LF of the blank
                        // line that ends the event.
                        let at = if i % 2 == 0 { f.len() / 2 } else { f.len() - 1 };
                        let (a, b) = f.split_at(at);
                        [
                            Step::Send(a.to_string()),
                            Step::Wait(GAP),
                            Step::Send(b.to_string()),
                        ]
                    })
                    .collect()
            }
            Says::CrlfStalls => sends(head.iter().map(|f| crlf(f)).collect())
                .chain([Step::Repeat(
                    vec![": PROC".into(), "ESSING\r\n\r\n".into()],
                    Duration::from_millis(50),
                )])
                .collect(),
        }
    }
}

fn crlf(frame: &str) -> String {
    frame.replace('\n', "\r\n")
}

fn block_start(index: u64, block: Value) -> String {
    anthropic_event(
        "content_block_start",
        json!({"type": "content_block_start", "index": index, "content_block": block}),
    )
}

fn block_delta(index: u64, delta: Value) -> String {
    anthropic_event(
        "content_block_delta",
        json!({"type": "content_block_delta", "index": index, "delta": delta}),
    )
}

fn text_delta(index: u64, text: &str) -> String {
    block_delta(index, json!({"type": "text_delta", "text": text}))
}

fn block_stop(index: u64) -> String {
    anthropic_event(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": index}),
    )
}

async fn chat(State(seen): State<Arc<Upstream>>) -> Response {
    let script = Wire::Chat.script(seen.says);
    stream(seen, script)
}

async fn legacy(State(seen): State<Arc<Upstream>>) -> Response {
    let script = Wire::Legacy.script(seen.says);
    stream(seen, script)
}

async fn messages(State(seen): State<Arc<Upstream>>) -> Response {
    let script = Wire::Anthropic.script(seen.says);
    stream(seen, script)
}

/// Play `script` as the body.
fn stream(seen: Arc<Upstream>, script: Vec<Step>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let open = Open(seen);
    tokio::spawn(async move {
        let _open = open;
        for step in script {
            match step {
                Step::Send(s) => {
                    if tx.send(Ok(Bytes::from(s))).await.is_err() {
                        return;
                    }
                }
                Step::Wait(d) => tokio::time::sleep(d).await,
                Step::Break => {
                    let _ = tx.send(Err(std::io::Error::other("the body broke"))).await;
                    return;
                }
                Step::Hold => return tx.closed().await,
                Step::Repeat(frames, every) => {
                    for f in frames.iter().cycle() {
                        tokio::time::sleep(every).await;
                        if tx.send(Ok(Bytes::from(f.clone()))).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

/// A gateway with [`ALIAS`] on a `protocol` row at `base` whose
/// `timeout_ms` is `timeout_ms`.
async fn gateway(base: &str, protocol: Protocol, timeout_ms: u64) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "silent-up".into(),
            protocol,
            kind: UpstreamKind::Generic,
            base_url: base.into(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: ALIAS.into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state.clone()).await;
    (state, gw)
}

/// A streamed request on each relay.
#[derive(Clone, Copy, Debug)]
enum Relay {
    Chat,
    Messages,
    Responses,
    Legacy,
}

impl Relay {
    const ALL: [Relay; 4] = [
        Relay::Chat,
        Relay::Messages,
        Relay::Responses,
        Relay::Legacy,
    ];

    fn request(self, gw: &Gw) -> reqwest::RequestBuilder {
        let (path, body) = match self {
            Relay::Chat => (
                "/v1/chat/completions",
                json!({"model": ALIAS, "stream": true,
                       "messages": [{"role": "user", "content": "ping"}]}),
            ),
            Relay::Messages => (
                "/v1/messages",
                json!({"model": ALIAS, "stream": true, "max_tokens": 64,
                       "messages": [{"role": "user", "content": "ping"}]}),
            ),
            Relay::Responses => (
                "/v1/responses",
                json!({"model": ALIAS, "stream": true, "input": "ping"}),
            ),
            Relay::Legacy => (
                "/v1/completions",
                json!({"model": ALIAS, "stream": true, "prompt": "ping"}),
            ),
        };
        let rb = gw.client().post(format!("{gw}{path}")).json(&body);
        match self {
            Relay::Messages => rb.header("anthropic-version", "2023-06-01"),
            _ => rb,
        }
    }
}

/// The whole stream's text, read to its end within a cut's time, and how
/// long that took.
async fn read_whole(rb: reqwest::RequestBuilder) -> (String, Duration) {
    let started = Instant::now();
    let resp = rb.send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let text = tokio::time::timeout(Duration::from_millis(TIMEOUT_MS) + SLACK, resp.text())
        .await
        .expect("the stream ended")
        .unwrap();
    (text, started.elapsed())
}

/// The events of an SSE text: `(event name, data)`.
fn events(text: &str) -> Vec<(Option<String>, String)> {
    lmgw_core::sse::SseDecoder::new()
        .feed(text.as_bytes())
        .into_iter()
        .map(|e| (e.event, e.data))
        .collect()
}

async fn rows(state: &SharedState) -> Vec<store::RequestLogRow> {
    store::query_logs(
        &state.db,
        &store::LogFilter {
            alias: Some(ALIAS.into()),
            limit: 50,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

/// The row of a request (the Responses loop's turn row, the one with an
/// error).
async fn row(state: &SharedState) -> store::RequestLogRow {
    patience::until_async("the stream's row", || async {
        rows(state).await.iter().any(|r| r.error_kind.is_some())
    })
    .await;
    rows(state)
        .await
        .into_iter()
        .find(|r| r.error_kind.is_some())
        .unwrap()
}

/// The rows of a request that ended whole: none of them an error.
async fn assert_whole_rows(state: &SharedState, what: &str) {
    patience::until_async("the stream's row", || async {
        !rows(state).await.is_empty()
    })
    .await;
    let rows = rows(state).await;
    assert!(
        rows.iter().all(|r| r.error_kind.is_none()),
        "{what} {rows:?}"
    );
}

/// The words every client and row is told.
const SILENT: &str = "the upstream sent nothing for 300 ms mid-stream";

/// The chat or legacy stream's error chunk, the last before `[DONE]`.
fn error_chunk(text: &str) -> Value {
    let evs = events(text);
    let (last, error) = (&evs[evs.len() - 1], &evs[evs.len() - 2]);
    assert_eq!(last.1, "[DONE]", "{text}");
    serde_json::from_str(&error.1).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// Each relay: the delta, then its protocol's error naming the silence, then
/// the end, within about the timeout; the upstream's connection closed; the
/// row a `timeout` with the same words.
#[tokio::test]
async fn a_stalled_upstream_ends_the_stream_with_an_error() {
    for relay in Relay::ALL {
        let (base, up) = upstream(Says::Stalls).await;
        let (state, gw) = gateway(&base, Protocol::OpenaiCompatible, TIMEOUT_MS).await;
        let (text, took) = read_whole(relay.request(&gw)).await;
        assert!(
            took >= Duration::from_millis(TIMEOUT_MS),
            "{relay:?} {took:?}"
        );
        assert!(text.contains("pong"), "{relay:?}: {text}");
        let evs = events(&text);
        match relay {
            Relay::Chat | Relay::Legacy => {
                let error = error_chunk(&text);
                assert_eq!(error["error"]["message"], SILENT, "{relay:?}: {text}");
                assert_eq!(error["error"]["type"], "api_error");
            }
            Relay::Messages => {
                let (name, data) = evs.last().unwrap();
                assert_eq!(name.as_deref(), Some("error"), "{text}");
                let data: Value = serde_json::from_str(data).unwrap();
                assert_eq!(
                    data,
                    json!({"type": "error", "error": {"type": "api_error", "message": SILENT}})
                );
                assert!(!text.contains("message_stop"), "{text}");
            }
            Relay::Responses => {
                let (name, data) = evs.last().unwrap();
                assert_eq!(name.as_deref(), Some("response.failed"), "{text}");
                let data: Value = serde_json::from_str(data).unwrap();
                assert_eq!(data["response"]["status"], "failed");
                assert_eq!(data["response"]["error"]["code"], "server_error");
                assert_eq!(data["response"]["error"]["message"], SILENT, "{text}");
            }
        }
        let row = row(&state).await;
        assert_eq!(
            (row.error_kind.as_deref(), row.error_msg.as_deref()),
            (Some("timeout"), Some(SILENT)),
            "{relay:?} {row:?}"
        );
        patience::until("the upstream stream to close", || {
            up.closed.load(Ordering::SeqCst) == 1
        })
        .await;
    }
}

/// `says` on every relay side by side, each relayed to its end, which takes
/// more than three timeouts, with no error on the wire or in a row; each
/// relay's text.
async fn uncut_on_every_relay(says: Says, protocol: Protocol, relays: &[Relay]) -> Vec<String> {
    let runs = relays.iter().map(|&relay| async move {
        let (base, _up) = upstream(says).await;
        let (state, gw) = gateway(&base, protocol, LONG_TIMEOUT_MS).await;
        let started = Instant::now();
        let text = relay
            .request(&gw)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            started.elapsed() > Duration::from_millis(LONG_TIMEOUT_MS * 3),
            "{relay:?} {:?}",
            started.elapsed()
        );
        assert!(!text.contains("sent nothing"), "{relay:?}: {text}");
        assert_whole_rows(&state, &format!("{says:?} {relay:?}")).await;
        text
    });
    futures::future::join_all(runs).await
}

/// An upstream that keeps producing, each gap under the timeout, is relayed
/// to its end however long it takes.
#[tokio::test]
async fn a_steady_upstream_is_not_cut() {
    let texts = uncut_on_every_relay(Says::Steady, Protocol::OpenaiCompatible, &Relay::ALL).await;
    for (relay, text) in Relay::ALL.iter().zip(texts) {
        assert_eq!(text.matches("\"a\"").count(), TRICKLES, "{relay:?}: {text}");
    }
}

/// A row whose timeout is 0 bounds no silence: the stalled stream is still
/// open well past what a 300 ms one would have been cut at.
#[tokio::test]
async fn a_timeout_of_zero_cuts_nothing() {
    for relay in Relay::ALL {
        let (base, up) = upstream(Says::Stalls).await;
        let (_state, gw) = gateway(&base, Protocol::OpenaiCompatible, 0).await;
        let resp = relay.request(&gw).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let read = tokio::time::timeout(Duration::from_millis(TIMEOUT_MS * 4), resp.text()).await;
        assert!(read.is_err(), "{relay:?}: the stream ended: {read:?}");
        assert_eq!(up.closed.load(Ordering::SeqCst), 0, "{relay:?}");
    }
}
