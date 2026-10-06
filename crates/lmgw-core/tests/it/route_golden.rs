//! The route corpus (llama-egress design §9.2, WP0): the upstream requests
//! every route makes, on a managed row (`support/gpu_world.rs`'s containers)
//! and on an external llama.cpp row (a wiremock upstream), captured before
//! the llama.cpp egress existed.
//!
//! Each case writes `tests/fixtures/egress_routes/<case>__<row>.json`: the
//! client-facing status, every POST the upstream got — path, headers (secrets
//! as `<set>`, without `host` and `content-length`), the body as its exact
//! string and parsed beside it — and, where the case is about it, the client's
//! response body. `LMGW_BLESS=1` rewrites the files (`support/golden.rs`).
//!
//! **The protocol name.** No fixture names a protocol, with one exception
//! kept in a field of its own: `chat_whole`'s `egress_proto`, the request
//! log's record of the wire a request left on (I4, WP2a). WP2a re-blesses
//! that field alone. The external row is stored under
//! [`LLAMA_SPELLING`](crate::egress_golden::LLAMA_SPELLING), the same one
//! place the egress corpus spells a llama.cpp upstream; [`external`] is the
//! only function that reads it. WP2a ran every route on that row spelled
//! the old way too (`openai` + `llama_server`) and found the same requests,
//! Continue and `/tokenize` aside (their guards key on `llama_cpp`); WP2c
//! removed the OpenAI egress's kind branches, so that spelling means a
//! generic row now.

use std::time::Duration;

use lmgw_core::config::{AuxKind, Settings};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::chat_actions::post;
use crate::chat_golden::{mcp_stub, openai_call, register_stub};
use crate::common::{serve, Gw};
use crate::egress_golden::LLAMA_SPELLING;
use crate::support::golden;
use crate::support::gpu_world::{Gpu, GIB};

/// The fixtures' directory under `tests/fixtures`.
const DIR: &str = "egress_routes";

/// Transport headers whose values carry a port or a length.
const TRANSPORT_HEADERS: [&str; 2] = ["host", "content-length"];

// ---------------------------------------------------------------------------
// The two rows
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// A chat row, an embedder and a reranker lmgw starts itself.
    Managed,
    /// An upstream row of llama.cpp's kind with a key and an extra header.
    External,
}

impl Row {
    pub(crate) const ALL: [Row; 2] = [Row::Managed, Row::External];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Row::Managed => "managed",
            Row::External => "external",
        }
    }
}

/// A gateway with one row of each class the routes need, on `row`.
pub(crate) struct World {
    pub state: SharedState,
    pub gw: Gw,
    gpu: Option<Gpu>,
    mock: Option<MockServer>,
    pub chat: &'static str,
    pub embed: &'static str,
    pub rerank: &'static str,
}

impl World {
    pub(crate) async fn new(row: Row) -> World {
        match row {
            Row::Managed => managed().await,
            Row::External => external().await,
        }
    }

    /// Every POST the upstream got so far, as the fixture records it.
    pub(crate) async fn posted(&self) -> Vec<Value> {
        let requests = match (&self.gpu, &self.mock) {
            (Some(gpu), _) => gpu.posted().await,
            (None, Some(mock)) => mock
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.method.as_str() == "POST")
                .collect(),
            (None, None) => unreachable!("a world has one upstream"),
        };
        requests.iter().map(recorded).collect()
    }

    /// The managed row's container world, for a case that changes how it
    /// answers.
    fn gpu(&self) -> Option<&Gpu> {
        self.gpu.as_ref()
    }
}

fn recorded(r: &Request) -> Value {
    let (body, body_json) = golden::body(&r.body);
    let path = match r.url.query() {
        Some(q) => format!("{}?{q}", r.url.path()),
        None => r.url.path().to_string(),
    };
    json!({
        "method": r.method.as_str(),
        "path": path,
        "headers": golden::headers(
            r.headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("<binary>").to_string())),
            &TRANSPORT_HEADERS,
        ),
        "body": body,
        "body_json": body_json,
    })
}

/// The Chat's default prompt, without the date the built-in one carries.
async fn fixed_chat_prompt(state: &SharedState) {
    let mut s: Settings = state.snapshot().settings.clone();
    s.set_default_chat_prompt("You are {{model}}. Be brief.");
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

/// `chat-model` (a thinking model, so a streamed request gets a stream), the
/// embedder `embed/emb` and the reranker `embed/rr`, each started on first
/// use in a container of its own.
async fn managed() -> World {
    let gpu = Gpu::new(8 * GIB, 3, 5).await;
    gpu.model("chat-model", GIB).await;
    gpu.aux("emb", AuxKind::Embed, GIB / 4).await;
    gpu.aux("rr", AuxKind::Rerank, GIB / 4).await;
    gpu.world().thinking.insert("chat-model".into());
    gpu.world().prompt_tokens = 5;
    fixed_chat_prompt(&gpu.state).await;
    let gw = serve(gpu.state.clone()).await;
    World {
        state: gpu.state.clone(),
        gw,
        gpu: Some(gpu),
        mock: None,
        chat: "chat-model",
        embed: "embed/emb",
        rerank: "embed/rr",
    }
}

/// A streamed answer from a llama.cpp server: reasoning, text, timings, usage.
const EXTERNAL_STREAM: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Let me think.\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"timings\":{\"prompt_n\":7,\"prompt_ms\":14.0,\"prompt_per_second\":500.0,\"predicted_n\":1,\"predicted_ms\":8.0,\"predicted_per_second\":125.0}}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":1}}\n\n",
    "data: [DONE]\n\n"
);

/// The external server's chat answer: a call of the MCP stub's `echo` when it
/// is offered and no tool result came back yet, else a stream or a whole
/// completion as asked.
fn external_chat(req: &Request) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
    let offers_echo = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|t| t["function"]["name"] == "stub__echo");
    let after_tool = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .is_some_and(|m| m["role"] == "tool");
    let sse = |s: String| ResponseTemplate::new(200).set_body_raw(s, "text/event-stream");
    if offers_echo && !after_tool {
        return sse(openai_call("", "c1", "stub__echo", "{\"text\":\"hi\"}"));
    }
    if body["stream"] == true {
        return sse(EXTERNAL_STREAM.into());
    }
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c", "object": "chat.completion", "created": 0, "model": "gguf",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "Hello",
                                 "reasoning_content": "Let me think."}}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 1, "total_tokens": 8},
        "timings": {"prompt_n": 7, "prompt_ms": 14.0, "prompt_per_second": 500.0,
                    "predicted_n": 1, "predicted_ms": 8.0, "predicted_per_second": 125.0},
    }))
}

/// An external llama.cpp server behind one upstream row, with the aliases
/// `m` (chat), `e` (embeddings) and `r` (rerank) on it.
async fn external() -> World {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(external_chat)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "type": "exceed_context_size_error",
            "message": "request (5000 tokens) exceeds the available context size",
            "n_prompt_tokens": 5000, "n_ctx": 4096,
        }})))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "model": "gguf",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.5, 0.25]},
                     {"object": "embedding", "index": 1, "embedding": [0.125, 1.0]}],
            "usage": {"prompt_tokens": 4, "total_tokens": 4},
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "gguf",
            "results": [{"index": 0, "relevance_score": 0.9}, {"index": 2, "relevance_score": 0.1}],
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens": [1, 2, 3, 4, 5]})))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/apply-template"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"prompt": "p"})))
        .mount(&mock)
        .await;

    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let (protocol, kind) = LLAMA_SPELLING;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "llama-ext".into(),
            protocol,
            kind,
            base_url: format!("{}/v1", mock.uri()),
            api_key: Some("sk-ext".into()),
            extra_headers: vec![("x-extra".into(), "one".into())],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    for (alias, model) in [
        ("m", "chat-gguf"),
        ("e", "embed-gguf"),
        ("r", "rerank-gguf"),
    ] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up,
                upstream_model_id: model.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    fixed_chat_prompt(&state).await;
    let gw = serve(state.clone()).await;
    World {
        state,
        gw,
        gpu: None,
        mock: Some(mock),
        chat: "m",
        embed: "e",
        rerank: "r",
    }
}

// ---------------------------------------------------------------------------
// The routes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(crate) enum Route {
    ChatWhole,
    ChatStream,
    ChatTools,
    Messages,
    Responses,
    ChatPageSend,
    ChatPageContinue,
    ChatPageToolThread,
    Embeddings,
    Rerank,
    CountTokens,
    MessagesCountTokens,
    Tokenize,
    CompletionsContextRefusal,
}

impl Route {
    pub(crate) const ALL: [Route; 14] = [
        Route::ChatWhole,
        Route::ChatStream,
        Route::ChatTools,
        Route::Messages,
        Route::Responses,
        Route::ChatPageSend,
        Route::ChatPageContinue,
        Route::ChatPageToolThread,
        Route::Embeddings,
        Route::Rerank,
        Route::CountTokens,
        Route::MessagesCountTokens,
        Route::Tokenize,
        Route::CompletionsContextRefusal,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Route::ChatWhole => "chat_whole",
            Route::ChatStream => "chat_stream",
            Route::ChatTools => "chat_tools",
            Route::Messages => "messages",
            Route::Responses => "responses",
            Route::ChatPageSend => "chat_page_send",
            Route::ChatPageContinue => "chat_page_continue",
            Route::ChatPageToolThread => "chat_page_tool_thread",
            Route::Embeddings => "embeddings",
            Route::Rerank => "rerank",
            Route::CountTokens => "count_tokens",
            Route::MessagesCountTokens => "messages_count_tokens",
            Route::Tokenize => "tokenize",
            Route::CompletionsContextRefusal => "completions_context_refusal",
        }
    }

    /// Run the route on `w`: what the fixture records.
    pub(crate) async fn run(self, w: &World) -> Value {
        let chat = w.chat;
        let (status, response) = match self {
            Route::ChatWhole => {
                let (status, _) = call(
                    w,
                    "/v1/chat/completions",
                    json!({"model": chat, "temperature": 0.5, "max_tokens": 64, "messages": [
                        {"role": "system", "content": "Be brief."},
                        {"role": "user", "content": "hello"},
                    ]}),
                )
                .await;
                let mut out = record(w, status, None).await;
                out["egress_proto"] = egress_protos(&w.state).await;
                return out;
            }
            Route::ChatStream => {
                call(
                    w,
                    "/v1/chat/completions",
                    json!({"model": chat, "stream": true, "top_k": 20, "messages": [
                        {"role": "user", "content": "hello"},
                    ]}),
                )
                .await
            }
            Route::ChatTools => {
                call(
                    w,
                    "/v1/chat/completions",
                    json!({"model": chat, "tool_choice": "auto", "messages": [
                        {"role": "user", "content": "Weather in Berlin?"},
                        {"role": "assistant", "content": null, "tool_calls": [
                            {"id": "call_1", "type": "function", "function": {
                                "name": "get_weather", "arguments": "{\"city\":\"Berlin\"}"}},
                        ]},
                        {"role": "tool", "tool_call_id": "call_1", "content": "12°C"},
                    ], "tools": [{"type": "function", "function": {
                        "name": "get_weather", "description": "The weather in a city.",
                        "parameters": {"type": "object",
                                       "properties": {"city": {"type": "string"}}},
                    }}]}),
                )
                .await
            }
            Route::Messages => {
                call(
                    w,
                    "/v1/messages",
                    json!({"model": chat, "max_tokens": 64, "system": "Be brief.",
                           "thinking": {"type": "enabled", "budget_tokens": 1024},
                           "messages": [
                        {"role": "user", "content": "Show me the weather chart."},
                        {"role": "assistant", "content": [
                            {"type": "text", "text": "Fetching it."},
                            {"type": "tool_use", "id": "toolu_1", "name": "chart",
                             "input": {"city": "Berlin"}},
                        ]},
                        {"role": "user", "content": [
                            {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                                {"type": "text", "text": "The chart:"},
                                {"type": "image", "source": {"type": "base64",
                                 "media_type": "image/png", "data": "iVBORw0KGgo="}},
                            ]},
                            {"type": "text", "text": "What does it show?"},
                        ]},
                    ], "tools": [{"name": "chart", "description": "A weather chart.",
                                  "input_schema": {"type": "object",
                                                   "properties": {"city": {"type": "string"}}}}]}),
                )
                .await
            }
            Route::Responses => {
                call(
                    w,
                    "/v1/responses",
                    json!({"model": chat, "instructions": "Be brief.", "store": false,
                           "reasoning": {"effort": "low"}, "max_output_tokens": 64,
                           "input": [{"role": "user", "content": [
                               {"type": "input_text", "text": "hello"}]}],
                           "tools": [{"type": "function", "name": "get_weather",
                                      "parameters": {"type": "object", "properties": {}}}]}),
                )
                .await
            }
            Route::ChatPageSend => {
                let tid = thread(w).await;
                call(
                    w,
                    &format!("/chat/api/threads/{tid}/send"),
                    json!({"content": "hi there"}),
                )
                .await
            }
            Route::ChatPageContinue => {
                let tid = thread(w).await;
                for (role, text) in [("user", "Tell me a story."), ("assistant", "Once upon a")] {
                    store::append_chat_message(&w.state.db, tid, role, text, "", None, None, None)
                        .await
                        .unwrap();
                }
                call(w, &format!("/chat/api/threads/{tid}/continue"), json!({})).await
            }
            Route::ChatPageToolThread => {
                if let Some(gpu) = w.gpu() {
                    gpu.world()
                        .calls_tool
                        .insert("chat-model".into(), "stub__echo".into());
                }
                register_stub(&w.state, &mcp_stub(Duration::ZERO).await).await;
                let tid = thread(w).await;
                let r = post(
                    &w.gw,
                    &format!("/chat/api/threads/{tid}/settings"),
                    json!({"mcp_tools": [{"server_label": "stub"}]}),
                )
                .await;
                assert_eq!(r.status(), 200);
                call(
                    w,
                    &format!("/chat/api/threads/{tid}/send"),
                    json!({"content": "say hi through the tool"}),
                )
                .await
            }
            Route::Embeddings => {
                call(
                    w,
                    "/v1/embeddings",
                    json!({"model": w.embed, "input": ["alpha", "beta"]}),
                )
                .await
            }
            Route::Rerank => {
                call(
                    w,
                    "/v1/rerank",
                    json!({"model": w.rerank, "query": "capital of France",
                           "documents": ["Paris", "Berlin", "Rome"], "top_n": 2}),
                )
                .await
            }
            Route::CountTokens => {
                call(
                    w,
                    "/v1/count_tokens",
                    json!({"model": chat, "input": "Hello, world!"}),
                )
                .await
            }
            Route::MessagesCountTokens => {
                call(
                    w,
                    "/v1/messages/count_tokens",
                    json!({"model": chat, "system": "Be brief.", "messages": [
                        {"role": "user", "content": "Hello, world!"},
                    ], "tools": [{"name": "chart", "description": "A weather chart.",
                                  "input_schema": {"type": "object", "properties": {}}}]}),
                )
                .await
            }
            Route::Tokenize => {
                call(
                    w,
                    "/tokenize",
                    json!({"model": chat, "content": "Hello, world!",
                           "add_special": false, "with_pieces": true}),
                )
                .await
            }
            Route::CompletionsContextRefusal => {
                if let Some(gpu) = w.gpu() {
                    gpu.world().refuse_context.insert("chat-model".into());
                }
                let (status, body) = call(
                    w,
                    "/v1/completions",
                    json!({"model": chat, "prompt": "Once upon a time", "max_tokens": 16}),
                )
                .await;
                let response = serde_json::from_str::<Value>(&body).unwrap_or(Value::String(body));
                (status, response.to_string())
            }
        };
        let keep = matches!(self, Route::CompletionsContextRefusal).then_some(response);
        record(w, status, keep).await
    }
}

/// POST `route` and read the whole answer, a stream included.
async fn call(w: &World, route: &str, body: Value) -> (u16, String) {
    let r = post(&w.gw, route, body).await;
    let status = r.status().as_u16();
    (status, r.text().await.unwrap())
}

/// A Chat thread on the world's chat model, carrying the fixed prompt.
async fn thread(w: &World) -> i64 {
    post(&w.gw, "/chat/api/threads", json!({"model_alias": w.chat}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

async fn record(w: &World, status: u16, response: Option<String>) -> Value {
    let mut out = json!({"status": status, "requests": w.posted().await});
    if let Some(r) = response {
        out["response"] = serde_json::from_str(&r).unwrap_or(Value::String(r));
    }
    out
}

/// The request log's `egress_proto` of every row so far, oldest first. The
/// row of a whole answer is written before the answer goes out; the wait is
/// for any route whose row is not.
async fn egress_protos(state: &SharedState) -> Value {
    for _ in 0..500 {
        let rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                limit: 50,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if !rows.is_empty() {
            return rows.iter().rev().map(|r| json!(r.egress_proto)).collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no request-log row was written");
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// Run `route` on both rows and compare each with its fixture.
async fn check(route: Route) {
    let mut failures = Vec::new();
    for row in Row::ALL {
        let w = World::new(row).await;
        let got = golden::to_fixture(&route.run(&w).await);
        let name = format!("{}__{}", route.name(), row.name());
        if let Err(e) = golden::check(DIR, &name, &got) {
            failures.push(e);
        }
        if !got.contains("\"egress_proto\"") {
            for word in ["openai", "llama_cpp", "llama_server", "anthropic", "gemini"] {
                assert!(
                    !got.to_ascii_lowercase().contains(word),
                    "{name} names '{word}'"
                );
            }
        }
    }
    golden::assert_all(route.name(), Row::ALL.len(), failures);
}

macro_rules! route_tests {
    ($($test:ident => $route:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $test() {
                check(Route::$route).await;
            }
        )*
    };
}

route_tests! {
    chat_whole => ChatWhole,
    chat_stream => ChatStream,
    chat_tools => ChatTools,
    messages => Messages,
    responses => Responses,
    chat_page_send => ChatPageSend,
    chat_page_continue => ChatPageContinue,
    chat_page_tool_thread => ChatPageToolThread,
    embeddings => Embeddings,
    rerank => Rerank,
    count_tokens => CountTokens,
    messages_count_tokens => MessagesCountTokens,
    tokenize => Tokenize,
    completions_context_refusal => CompletionsContextRefusal,
}

/// No fixture outlives its case.
#[test]
fn no_route_fixture_is_stale() {
    let written = Route::ALL
        .iter()
        .flat_map(|r| Row::ALL.map(|row| format!("{}__{}", r.name(), row.name())))
        .collect();
    golden::assert_all("route", Route::ALL.len() * 2, golden::stale(DIR, &written));
}
