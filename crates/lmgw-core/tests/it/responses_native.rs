//! `/v1/responses` on an upstream that implements it (openai egress design
//! §5.2, §6, WP5): the OpenAI platform, and an `openai_compatible` row with
//! the flag.
//!
//! - [`metering`]: a native answer's row carries the tokens and cost its
//!   Response object reported, whole and streamed, and a budget sees it.
//! - [`hangup`]: a client that goes away still leaves the row.
//! - [`routing`]: a request only lmgw's own loop can serve stays in the loop
//!   (an `mcp` label lmgw serves, a `previous_response_id` lmgw stored);
//!   anything else goes native.
//! - [`scoping`]: a client key cannot chain from, or name the conversation
//!   of, anything that is not its own, on the native path either.
//! - [`retrieve`]: a native response can be fetched, listed and deleted
//!   through lmgw, and every stored response is its key's.
//! - [`hosted`]: hosted tools and the other parts only a native upstream
//!   serves go there as sent, are refused by the loop, and leave their row
//!   unpriced when they bill beyond their tokens (WP5b).
//! - [`tier`]: the platform's answer is priced at the service tier its
//!   Response object reports, and one its tier has no row for is unpriced
//!   with a price note (WP4).
//! - [`budget_floor`]: budget lower bounds (§13, WP9): flex without a flex
//!   price is refused to a budgeted caller, and a stream cut before its
//!   terminal event floors from a local count.
//!
//! The upstream is a small axum server ([`Upstream`]) that answers both
//! `/v1/responses` and `/v1/chat/completions` and tells lmgw's own loop
//! turns from native forwards, so a test can tell which path a request
//! took, and holds a stream open when told to. On the platform a loop turn
//! is a `/v1/responses` call too (the platform's chat sends take the
//! Responses wire, openai egress design §7.2): it is the one that asks for a
//! reasoning summary, which no native request here does.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use lmgw_core::config::{PriceScope, PriceUnit, Protocol, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};

use crate::common::{self, patience, Gw};

mod budget_floor;
mod hangup;
mod hosted;
mod hosted_fees;
mod metering;
mod retrieve;
mod routing;
mod scoping;
mod silence;
mod tier;

/// The alias every test asks for.
const ALIAS: &str = "gpt";

/// The usage every native answer here reports: 1000 input tokens, 600 of them
/// read from the prompt cache and 100 written to it, and 200 output tokens,
/// 150 of them reasoning.
fn usage() -> Value {
    json!({
        "input_tokens": 1000,
        "input_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 100},
        "output_tokens": 200,
        "output_tokens_details": {"reasoning_tokens": 150},
        "total_tokens": 1200,
    })
}

/// What [`usage`] costs under [`price`]'s sheet, in micro-units: 300 plain
/// input × 2 + 600 cached × 0.5 + 100 written × 2.5 + 200 output × 8.
const COST_MICRO: i64 = 600 + 300 + 250 + 1600;

/// How the upstream answers `/v1/responses`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Says {
    /// The whole answer, its usage in the body or in `response.completed`.
    Answers,
    /// A stream's head and one delta, then nothing until its connection
    /// closes.
    Holds,
    /// A stream's head and one delta, then its end: cut before
    /// `response.completed`.
    Cuts,
    /// The whole stream to `response.completed`, then the connection held
    /// open until it closes.
    CompletesThenHolds,
    /// A whole answer, sent only after [`SLOW_ANSWER`]; a connection closed
    /// before then counts in [`Upstream::closed`].
    Slowly,
    /// A whole answer without `usage`.
    NoUsage,
    /// The whole stream to `response.completed`, then a broken body.
    CompletesThenBreaks,
    /// As [`Says::Answers`], its Response object served at the tier given
    /// (`None`: it names none).
    AnswersAt(Option<&'static str>),
    /// As [`Says::Answers`], its `output` led by one item of each type
    /// given (`web_search_call`, …): the calls a hosted tool made.
    AnswersWith(&'static [&'static str]),
    /// A stream's head, one finished `web_search_call` item, then its end:
    /// cut before `response.completed`, after a search.
    SearchesThenCuts,
    /// A stream's head and one delta, then `response.failed` whose `usage`
    /// is null.
    FailsWithoutUsage,
    /// A stream's head and one delta, then only a keep-alive comment every
    /// [`KEEP_ALIVE`] until its connection closes: a provider stalled behind
    /// its aggregator (`silence`).
    HoldsAlive,
    /// A stream's head, then [`TRICKLES`] more deltas, each the given gap
    /// after the last, then `response.completed`.
    Trickles(Duration),
    /// A stream's head and one delta, then a `reasoning` item added and
    /// [`TRICKLES`] keep-alive comments, each the given gap after the last,
    /// then the item done, a delta and `response.completed`: a model
    /// thinking out of sight behind its aggregator.
    ReasonsHidden(Duration),
    /// As [`Says::ReasonsHidden`], the item a `web_search_call`: a hosted
    /// search running behind its aggregator.
    SearchesHidden(Duration),
    /// A stream's head and one delta, then half an event, then nothing
    /// until its connection closes.
    HalfThenHolds,
    /// A stream's head and one delta, then half an event, then a broken
    /// body.
    HalfThenBreaks,
    /// A stream's head and one delta, then an `error` event, then nothing
    /// until its connection closes.
    ErrsThenHolds,
}

/// How often a [`Says::HoldsAlive`] upstream writes its keep-alive.
const KEEP_ALIVE: Duration = Duration::from_millis(50);

/// The frames a [`Says::Trickles`] or [`Says::ReasonsHidden`] upstream
/// trickles.
const TRICKLES: usize = 14;

/// What a [`Says::HalfThenHolds`] or [`Says::HalfThenBreaks`] upstream
/// sends of its last event.
const HALF_EVENT: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.outp";

impl Says {
    /// The finished Response object this upstream sends.
    fn finished(self) -> Value {
        match self {
            Says::AnswersAt(tier) => response_object_at(tier),
            Says::AnswersWith(items) => {
                let mut v = response_object();
                let mut output: Vec<Value> = items
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let mut item =
                            json!({"type": t, "id": format!("call_{i}"), "status": "completed"});
                        // A reasoning item comes with its encrypted content,
                        // as the platform's does with `store: false`.
                        if *t == "reasoning" {
                            item["summary"] = json!([]);
                            item["encrypted_content"] = json!("gAAA");
                        }
                        item
                    })
                    .collect();
                output.extend(v["output"].as_array().cloned().unwrap_or_default());
                v["output"] = Value::Array(output);
                v
            }
            _ => response_object(),
        }
    }
}

/// How long a [`Says::Slowly`] answer takes.
const SLOW_ANSWER: Duration = Duration::from_millis(400);

/// The upstream, and what it saw.
struct Upstream {
    says: Says,
    /// `/v1/responses` requests, their bodies in arrival order.
    responses: Mutex<Vec<Value>>,
    /// lmgw's loop turns: `/v1/chat/completions` requests, and on the
    /// platform the `/v1/responses` ones its egress built ([`loop_turn`]).
    chats: AtomicUsize,
    /// Streams whose connection closed before they ended.
    closed: AtomicUsize,
    /// The retrievals forwarded to it: method, path and query.
    fetched: Mutex<Vec<String>>,
}

impl Upstream {
    fn responses(&self) -> Vec<Value> {
        self.responses.lock().unwrap().clone()
    }

    fn chats(&self) -> usize {
        self.chats.load(Ordering::SeqCst)
    }
}

/// Counts a stream as closed when dropped before it ended.
struct Open(Arc<Upstream>, bool);

impl Drop for Open {
    fn drop(&mut self) {
        if !self.1 {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Serve an upstream that `says`; its base URL ends in `/v1`.
async fn upstream(says: Says) -> (String, Arc<Upstream>) {
    let seen = Arc::new(Upstream {
        says,
        responses: Mutex::new(Vec::new()),
        chats: AtomicUsize::new(0),
        closed: AtomicUsize::new(0),
        fetched: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/v1/responses", axum::routing::post(respond))
        .route(
            "/v1/responses/{id}",
            axum::routing::get(fetch).delete(fetch),
        )
        .route("/v1/responses/{id}/input_items", axum::routing::get(fetch))
        .route("/v1/chat/completions", axum::routing::post(chat))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

/// lmgw's loop asking for a turn: one plain answer.
async fn chat(State(seen): State<Arc<Upstream>>) -> Json<Value> {
    seen.chats.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-test",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "from the loop"}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
        "service_tier": "default",
    }))
}

/// The Response object a finished answer carries: served at Standard, which
/// OpenAI's says as `default` (openai egress design §5.4).
fn response_object() -> Value {
    response_object_at(Some("default"))
}

/// [`response_object`], served at `tier` (`None`: the object names none).
fn response_object_at(tier: Option<&str>) -> Value {
    let mut v = json!({
        "id": "resp_upstream", "object": "response", "created_at": 0, "status": "completed",
        "model": "gpt-test",
        "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "Hello", "annotations": []}]}],
        "usage": usage(),
    });
    if let Some(t) = tier {
        v["service_tier"] = json!(t);
    }
    v
}

/// A retrieval lmgw forwarded: noted, and answered with what names it — as
/// a page with a script in it when the query asks `as=html`, which is what an
/// upstream (or anything in its place) is free to send.
async fn fetch(
    State(seen): State<Arc<Upstream>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
) -> Response {
    let what = format!("{method} {uri}");
    seen.fetched.lock().unwrap().push(what.clone());
    if uri.query().is_some_and(|q| q.contains("as=html")) {
        return (
            [("content-type", "text/html; charset=utf-8")],
            format!("<p>{what}</p><script>fetch('/api/op/settings_set')</script>"),
        )
            .into_response();
    }
    Json(json!({"upstream_saw": what})).into_response()
}

fn event(name: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

/// Whether a `/v1/responses` body is a turn of lmgw's loop on the platform:
/// its egress asks for a reasoning summary on every send, and no native
/// request here does.
fn loop_turn(body: &Value) -> bool {
    body.pointer("/reasoning/summary") == Some(&json!("auto"))
}

/// The loop turn's answer on the Responses wire: the chat answer's words,
/// usage and tier, whole or streamed as asked.
fn loop_answer(body: &Value) -> Response {
    let answer = json!({
        "id": "resp_loop", "object": "response", "created_at": 0, "status": "completed",
        "model": "gpt-test", "service_tier": "default",
        "output": [crate::support::responses_wire::message("msg_loop", "from the loop")],
        "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8},
    });
    if body["stream"] != json!(true) {
        return Json(answer).into_response();
    }
    let sse = crate::support::responses_wire::sse(&[
        json!({"type": "response.output_text.delta", "output_index": 0,
               "delta": "from the loop"}),
        json!({"type": "response.completed", "response": answer}),
    ]);
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

async fn respond(State(seen): State<Arc<Upstream>>, Json(body): Json<Value>) -> Response {
    if loop_turn(&body) {
        seen.chats.fetch_add(1, Ordering::SeqCst);
        return loop_answer(&body);
    }
    seen.responses.lock().unwrap().push(body.clone());
    if body["stream"] != json!(true) {
        if seen.says == Says::Slowly {
            let mut open = Open(seen.clone(), false);
            tokio::time::sleep(SLOW_ANSWER).await;
            open.1 = true;
        }
        let mut answer = seen.says.finished();
        if seen.says == Says::NoUsage {
            answer.as_object_mut().unwrap().remove("usage");
        }
        return Json(answer).into_response();
    }
    let says = seen.says;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let open = Open(seen.clone(), false);
    tokio::spawn(async move {
        let mut open = open;
        let mut head = response_object();
        head["status"] = json!("in_progress");
        head["usage"] = Value::Null;
        let frames = [
            event(
                "response.created",
                json!({"type": "response.created", "response": head}),
            ),
            event(
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "delta": "Hel"}),
            ),
        ];
        for f in frames {
            if tx.send(Ok(f)).await.is_err() {
                return;
            }
        }
        if says == Says::Holds {
            return tx.closed().await;
        }
        if says == Says::HoldsAlive {
            loop {
                tokio::time::sleep(KEEP_ALIVE).await;
                if tx.send(Ok(Bytes::from(": PROCESSING\n\n"))).await.is_err() {
                    return;
                }
            }
        }
        if matches!(says, Says::HalfThenHolds | Says::HalfThenBreaks) {
            if tx.send(Ok(Bytes::from(HALF_EVENT))).await.is_err() {
                return;
            }
            if says == Says::HalfThenHolds {
                return tx.closed().await;
            }
            // Long enough for the half to have left: an error ends the
            // body at once, buffered bytes with it.
            tokio::time::sleep(Duration::from_millis(200)).await;
            open.1 = true;
            let _ = tx.send(Err(std::io::Error::other("the body broke"))).await;
            return;
        }
        if says == Says::ErrsThenHolds {
            let ev = event(
                "error",
                json!({"type": "error", "code": "server_error", "message": "overloaded",
                       "param": null, "sequence_number": 2}),
            );
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
            return tx.closed().await;
        }
        if let Says::ReasonsHidden(gap) | Says::SearchesHidden(gap) = says {
            let item = match says {
                Says::SearchesHidden(_) => {
                    json!({"type": "web_search_call", "id": "ws_1", "status": "in_progress"})
                }
                _ => json!({"type": "reasoning", "id": "rs_1", "summary": []}),
            };
            let added = event(
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 1, "item": item}),
            );
            if tx.send(Ok(added)).await.is_err() {
                return;
            }
            for _ in 0..TRICKLES {
                tokio::time::sleep(gap).await;
                if tx.send(Ok(Bytes::from(": PROCESSING\n\n"))).await.is_err() {
                    return;
                }
            }
            let done = event(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "output_index": 1, "item": item}),
            );
            let delta = event(
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "delta": "lo"}),
            );
            for f in [done, delta] {
                if tx.send(Ok(f)).await.is_err() {
                    return;
                }
            }
        }
        if let Says::Trickles(gap) = says {
            for _ in 0..TRICKLES {
                tokio::time::sleep(gap).await;
                let delta = event(
                    "response.output_text.delta",
                    json!({"type": "response.output_text.delta", "delta": "lo"}),
                );
                if tx.send(Ok(delta)).await.is_err() {
                    return;
                }
            }
        }
        if says == Says::Cuts {
            open.1 = true;
            return;
        }
        if says == Says::FailsWithoutUsage {
            let mut failed = response_object();
            failed["status"] = json!("failed");
            failed["usage"] = Value::Null;
            failed["error"] = json!({"code": "server_error", "message": "it broke"});
            let ev = event(
                "response.failed",
                json!({"type": "response.failed", "response": failed}),
            );
            let _ = tx.send(Ok(ev)).await;
            open.1 = true;
            return;
        }
        if says == Says::SearchesThenCuts {
            let item = event(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "output_index": 0,
                       "item": {"type": "web_search_call", "id": "ws_1", "status": "completed"}}),
            );
            let _ = tx.send(Ok(item)).await;
            open.1 = true;
            return;
        }
        let done = event(
            "response.completed",
            json!({"type": "response.completed", "response": says.finished()}),
        );
        if tx.send(Ok(done)).await.is_err() {
            return;
        }
        if says == Says::CompletesThenHolds {
            return tx.closed().await;
        }
        if says == Says::CompletesThenBreaks {
            open.1 = true;
            // Long enough for the event to have left: an error ends the body
            // at once, buffered bytes with it.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tx.send(Err(std::io::Error::other("the body broke"))).await;
            return;
        }
        open.1 = true;
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

/// A gateway with [`ALIAS`] on an upstream row of `protocol` at `base`
/// (`supports_responses` set for an `openai_compatible` row), priced per
/// token on the alias.
async fn gateway(protocol: Protocol, base: &str) -> (SharedState, Gw) {
    gateway_flagged(protocol, base, protocol == Protocol::OpenaiCompatible).await
}

/// [`gateway`] with `supports_responses` as given: an `openai_compatible`
/// row without it does not forward `/v1/responses`.
async fn gateway_flagged(protocol: Protocol, base: &str, flag: bool) -> (SharedState, Gw) {
    gateway_row(protocol, base, flag, 30_000).await
}

/// [`gateway`] on an upstream row whose `timeout_ms` is `timeout_ms`.
async fn gateway_timing_out(protocol: Protocol, base: &str, timeout_ms: u64) -> (SharedState, Gw) {
    gateway_row(
        protocol,
        base,
        protocol == Protocol::OpenaiCompatible,
        timeout_ms,
    )
    .await
}

async fn gateway_row(
    protocol: Protocol,
    base: &str,
    flag: bool,
    timeout_ms: u64,
) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "native-up".into(),
            protocol,
            kind: UpstreamKind::Generic,
            base_url: base.into(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: flag,
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
            upstream_model_id: "gpt-test".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    price(&state).await;
    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state.clone()).await;
    (state, gw)
}

/// Per 1M tokens: 2 in, 0.5 cached, 2.5 written to the cache, 8 out.
async fn price(state: &SharedState) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        ALIAS,
        PriceUnit::PerMtok,
        lmgw_core::config::PriceTier::Standard,
        &Prices {
            price_in: Some(2.0),
            price_out: Some(8.0),
            price_cache_read: Some(0.5),
            price_cache_write: Some(2.5),
            source: PriceSource::Manual,
        },
        None,
        None,
    )
    .await
    .unwrap();
}

/// What the budgets count for a native stream cut before its terminal
/// event (openai egress design §13.4), whose body sent `prompt` and whose
/// deltas relayed `relayed`, under [`price`]'s sheet: the whole prompt at
/// the cache-read rate (0.5), the output at 8. The clause its note ends
/// with, and the floor in micro-units. Counted as the platform counts:
/// tiktoken, `o200k_base` for a model it does not know.
fn cut_floor(prompt: &str, relayed: &str) -> (String, i64) {
    let bpe = tiktoken_rs::bpe_for_model("gpt-test")
        .or_else(|_| tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase))
        .unwrap();
    let (p, o) = (
        bpe.encode_ordinary(prompt).len() as i64,
        bpe.encode_ordinary(relayed).len() as i64,
    );
    let micro = (p as f64 * 0.5).round() as i64 + o * 8;
    (
        format!(
            "budgets count a local count of {p} prompt and {o} output tokens at the standard \
             price, {:.6} USD, a lower bound",
            micro as f64 / 1_000_000.0
        ),
        micro,
    )
}

/// Both protocols that forward `/v1/responses` natively.
const NATIVE: [Protocol; 2] = [Protocol::Openai, Protocol::OpenaiCompatible];

/// `POST /v1/responses` with `body`, read whole: its status and JSON.
async fn post(gw: &Gw, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// The `n`th request-log row for [`ALIAS`] (1 = the first), once there are
/// `n`: a native row is written when its answer has ended.
async fn row(state: &SharedState, n: usize) -> store::RequestLogRow {
    let rows = || async {
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
    };
    patience::until_async(&format!("request-log row {n} for '{ALIAS}'"), || async {
        rows().await.len() >= n
    })
    .await;
    let mut all = rows().await;
    all.reverse();
    all.swap_remove(n - 1)
}

/// The row of a native answer that reported [`usage`].
fn assert_metered(row: &store::RequestLogRow) {
    assert_eq!(row.ingress_proto, "responses", "{row:?}");
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens),
        (Some(1000), Some(200)),
        "the totals, the cache inside the prompt: {row:?}"
    );
    assert_eq!(
        (
            row.cached_in_tokens,
            row.cache_write_tokens,
            row.reasoning_tokens
        ),
        (Some(600), Some(100), Some(150)),
        "{row:?}"
    );
    assert_eq!(row.cost_micro, Some(COST_MICRO), "{row:?}");
    assert_eq!(row.price_source.as_deref(), Some("manual"), "{row:?}");
}
