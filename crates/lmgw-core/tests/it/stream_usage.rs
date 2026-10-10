//! `stream_options.include_usage` on a streamed `/v1/chat/completions`, as
//! OpenAI defines it: unasked (absent or `false`), no chunk carries `usage`
//! and none has empty `choices`, so the SDK loop that reads `choices[0]`
//! holds to the end; asked, every chunk says `usage: null` and the last one
//! before `[DONE]` has `choices: []` and the usage. The upstream is asked for
//! usage every time, so the request row records the tokens every time.
//!
//! One case per upstream protocol, because each decoder hands the encoder its
//! usage at a different point (a trailing chunk, `message_delta`, the finish
//! chunk's `usageMetadata`). The managed local models, a hold's fallback and
//! a ladder's climb are in `vram_admission::stream_usage`.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, RequestLogRow};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::e2e_proxy::setup_kind;

/// What a request says about `stream_options.include_usage`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Ask {
    Absent,
    False,
    True,
}

impl Ask {
    pub(crate) const ALL: [Ask; 3] = [Ask::Absent, Ask::False, Ask::True];

    /// A streamed chat body for `model` saying it.
    pub(crate) fn body(self, model: &str, content: &str) -> Value {
        let mut body = json!({
            "model": model, "stream": true, "max_tokens": 16,
            "messages": [{"role": "user", "content": content}],
        });
        match self {
            Ask::Absent => {}
            Ask::False => body["stream_options"] = json!({"include_usage": false}),
            Ask::True => body["stream_options"] = json!({"include_usage": true}),
        }
        body
    }

    pub(crate) fn wants_usage(self) -> bool {
        matches!(self, Ask::True)
    }
}

/// Every frame of `POST /v1/chat/completions`'s stream validated against the
/// schema the built document lists for its `chunk` event.
pub(crate) fn frames_validate(frames: &[Value]) {
    let doc = lmgw_core::openapi::admin_doc();
    let schema = doc
        .pointer("/paths/~1v1~1chat~1completions/post/responses/200/x-lmgw-sse-events/chunk")
        .expect("the chat completions stream lists a chunk event");
    for frame in frames {
        crate::common::validates_against("/v1/chat/completions chunk", doc, schema, frame);
    }
}

/// The JSON chunks of an OpenAI SSE answer, which must end with `[DONE]`;
/// each is validated against the document's chunk schema.
pub(crate) fn chunks(text: &str) -> Vec<Value> {
    let events = lmgw_core::sse::SseDecoder::new().feed(text.as_bytes());
    let (done, rest) = events.split_last().expect("a non-empty stream");
    assert_eq!(done.data, "[DONE]", "{text}");
    let frames: Vec<Value> = rest
        .iter()
        .map(|e| serde_json::from_str(&e.data).unwrap_or_else(|_| panic!("not JSON: {}", e.data)))
        .collect();
    frames_validate(&frames);
    frames
}

/// OpenAI's rule for `ask`, with `(prompt, completion)` the usage the last
/// chunk must carry when it was asked for.
pub(crate) fn assert_usage_rule(chunks: &[Value], ask: Ask, (prompt, completion): (u64, u64)) {
    let (last, rest) = chunks.split_last().expect("at least one chunk");
    let has_choice = |c: &Value| c["choices"].as_array().is_some_and(|a| !a.is_empty());
    if ask.wants_usage() {
        assert_eq!(last["choices"], json!([]), "{ask:?}: {last}");
        assert_eq!(last["object"], "chat.completion.chunk");
        assert_eq!(
            last["usage"],
            json!({"prompt_tokens": prompt, "completion_tokens": completion,
                   "total_tokens": prompt + completion}),
            "{ask:?}"
        );
        for c in rest {
            assert!(c.get("usage").is_some_and(Value::is_null), "{ask:?}: {c}");
            assert!(has_choice(c), "{ask:?}: {c}");
        }
    } else {
        for c in chunks {
            assert!(c.get("usage").is_none(), "{ask:?}: {c}");
            assert!(has_choice(c), "{ask:?}: {c}");
        }
        assert_eq!(last["choices"][0]["finish_reason"], "stop", "{ask:?}");
    }
}

/// The answer's text, joined from every chunk's `delta.content`.
pub(crate) fn text_of(chunks: &[Value]) -> String {
    chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect()
}

/// An SSE answer with `body` as its frames.
pub(crate) fn sse(body: &'static str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(body, "text/event-stream")
}

/// llama-server's stream when the request asks for usage, as the gateway
/// always does: the finish chunk, then a usage chunk with `choices: []` that
/// also carries the timings.
pub(crate) const LLAMA_SSE: &str = concat!(
    "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null}}],\"object\":\"chat.completion.chunk\"}\n\n",
    "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"delta\":{\"content\":\"Hel\"}}],\"object\":\"chat.completion.chunk\"}\n\n",
    "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"delta\":{\"content\":\"lo\"}}],\"object\":\"chat.completion.chunk\"}\n\n",
    "data: {\"choices\":[{\"finish_reason\":\"stop\",\"index\":0,\"delta\":{}}],\"object\":\"chat.completion.chunk\"}\n\n",
    "data: {\"choices\":[],\"object\":\"chat.completion.chunk\",\"usage\":{\"completion_tokens\":2,\"prompt_tokens\":7,\"total_tokens\":9},",
    "\"timings\":{\"prompt_n\":7,\"prompt_ms\":3.1,\"prompt_per_second\":2258.0,\"predicted_n\":2,\"predicted_ms\":10.0,\"predicted_per_second\":200.0}}\n\n",
    "data: [DONE]\n\n",
);

/// OpenAI's own stream with `include_usage`, which is what the gateway asks
/// an OpenAI-compatible upstream for: `usage: null` on every chunk, then the
/// usage chunk.
pub(crate) const OPENAI_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-up\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-up\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-up\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-up\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-up\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n",
    "data: [DONE]\n\n",
);

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"tgt-model\",\"content\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Gemini repeats `usageMetadata` on every chunk; the finishing one has the
/// totals.
const GEMINI_SSE: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hel\"}]}}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":1,\"totalTokenCount\":8}}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"lo\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":2,\"totalTokenCount\":9}}\n\n",
);

/// The newest request row, once there are more than `before`: a stream writes
/// its row when the relay ends, after the client has its last byte.
pub(crate) async fn row_after(state: &SharedState, before: usize) -> RequestLogRow {
    for _ in 0..200 {
        let rows = store::query_logs(&state.db, &Default::default())
            .await
            .unwrap();
        if rows.len() > before {
            return rows.into_iter().next().unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no new request row");
}

async fn row_count(state: &SharedState) -> usize {
    store::query_logs(&state.db, &Default::default())
        .await
        .unwrap()
        .len()
}

/// The body of the last request posted to `mock`.
pub(crate) async fn last_post(mock: &MockServer) -> Value {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .rev()
        .find(|r| r.method == wiremock::http::Method::POST)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .expect("a request posted to the mock")
}

/// Every [`Ask`] through the gateway to an upstream of `protocol` that
/// answers `answer` on `route`: the client gets OpenAI's rule, the row gets
/// 7 + 2 tokens. `asks_upstream`: the upstream speaks OpenAI's
/// `stream_options`, and is asked for usage whatever the client said.
async fn each_ask(
    protocol: Protocol,
    kind: UpstreamKind,
    route: &str,
    answer: &'static str,
    asks_upstream: bool,
) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(sse(answer))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), protocol, kind).await;
    for ask in Ask::ALL {
        let before = row_count(&state).await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&ask.body("my-model", "hi"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{ask:?}");
        let chunks = chunks(&resp.text().await.unwrap());
        assert_usage_rule(&chunks, ask, (7, 2));
        assert_eq!(text_of(&chunks), "Hello", "{ask:?}");
        assert!(chunks.iter().all(|c| c["model"] == "my-model"), "{ask:?}");

        let row = row_after(&state, before).await;
        assert!(row.streamed);
        assert_eq!(row.status, 200);
        assert!(row.error_kind.is_none(), "{ask:?}: {row:?}");
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens),
            (Some(7), Some(2)),
            "{ask:?}: the row has the tokens, asked for or not"
        );
        if asks_upstream {
            assert_eq!(
                last_post(&mock).await["stream_options"],
                json!({"include_usage": true}),
                "{ask:?}: the upstream is asked for usage regardless"
            );
        }
    }
}

#[tokio::test]
async fn an_openai_upstream_stream_shows_usage_only_when_asked() {
    each_ask(
        Protocol::Openai,
        UpstreamKind::Generic,
        "/chat/completions",
        OPENAI_SSE,
        true,
    )
    .await;
}

#[tokio::test]
async fn a_llama_cpp_upstream_stream_shows_usage_only_when_asked() {
    each_ask(
        Protocol::LlamaCpp,
        UpstreamKind::LlamaServer,
        "/chat/completions",
        LLAMA_SSE,
        true,
    )
    .await;
}

#[tokio::test]
async fn an_anthropic_upstream_stream_shows_usage_only_when_asked() {
    each_ask(
        Protocol::Anthropic,
        UpstreamKind::Generic,
        "/v1/messages",
        ANTHROPIC_SSE,
        false,
    )
    .await;
}

#[tokio::test]
async fn a_gemini_upstream_stream_shows_usage_only_when_asked() {
    each_ask(
        Protocol::Gemini,
        UpstreamKind::Generic,
        "/v1beta/models/tgt-model:streamGenerateContent",
        GEMINI_SSE,
        false,
    )
    .await;
}

/// Legacy `/v1/completions` is a byte pipe to an OpenAI-protocol upstream:
/// the client's `stream_options` reach it exactly as sent (absent stays
/// absent, nothing is added), and its answer comes back untouched — so the
/// rule is the upstream's own, applied to what the client asked.
#[tokio::test]
async fn legacy_completions_forward_the_clients_stream_options_as_sent() {
    const ANSWER: &str = concat!(
        "data: {\"object\":\"text_completion\",\"choices\":[{\"index\":0,\"text\":\"Hel\",\"finish_reason\":null}]}\n\n",
        "data: {\"object\":\"text_completion\",\"choices\":[{\"index\":0,\"text\":\"lo\",\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(sse(ANSWER))
        .mount(&mock)
        .await;
    let (_state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    for ask in Ask::ALL {
        let mut body = ask.body("my-model", "hi");
        let obj = body.as_object_mut().unwrap();
        obj.remove("messages");
        obj.insert("prompt".into(), json!("hi"));
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/completions"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{ask:?}");
        assert_eq!(resp.text().await.unwrap(), ANSWER, "{ask:?}");
        assert_eq!(
            last_post(&mock).await.get("stream_options"),
            body.get("stream_options"),
            "{ask:?}"
        );
    }
}

/// An upstream that fails after it began: its error frame comes out as the
/// stream's own error frame, which the document lists beside the chunk.
#[tokio::test]
async fn a_failure_after_the_first_chunk_is_a_documented_error_frame() {
    const FAILING: &str = concat!(
        "data: {\"id\":\"chatcmpl-up\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"error\":{\"message\":\"overloaded\",\"type\":\"server_error\"}}\n\n",
        "data: [DONE]\n\n",
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse(FAILING))
        .mount(&mock)
        .await;
    let (_state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    for ask in [Ask::Absent, Ask::True] {
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&ask.body("my-model", "hi"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{ask:?}");
        let text = resp.text().await.unwrap();
        let frames: Vec<Value> = lmgw_core::sse::SseDecoder::new()
            .feed(text.as_bytes())
            .iter()
            .filter(|e| e.data != "[DONE]")
            .map(|e| serde_json::from_str(&e.data).unwrap())
            .collect();
        let error = frames
            .iter()
            .find(|f| f.get("error").is_some())
            .unwrap_or_else(|| panic!("{ask:?}: no error frame in {text}"));
        assert_eq!(error["error"]["message"], "overloaded");
        frames_validate(&frames);
    }
}
