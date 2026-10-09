//! Gemini thought signatures through lmgw (gateway design §7.1): a model's
//! `thoughtSignature` rides out in the id of the call it signed, comes back
//! in the client's next request on each client shape (OpenAI chat
//! completions, Anthropic messages, Responses), streamed or not, and goes
//! back to Gemini on its own part; the first call of any step that has none
//! (or whose id a client damaged) gets the documented skip value; an
//! upstream other than Gemini sees the bare id, on the synthesized sends and
//! on the routes that forward the client's body (the native Responses
//! passthrough, Anthropic's count); every call id Gemini's answers get is
//! unique across steps; and a Chat thread keeps the signature in its stored
//! record.
//!
//! `tests/fixtures/gemini_signatures/<case>__<ingress>.json` hold the
//! request bodies Gemini receives; `LMGW_BLESS=1` rewrites them.

mod capture;
mod chat;
mod ids;
mod passthrough;
mod replay;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::THOUGHT_SIGNATURE_MARKER;
use serde_json::{json, Value};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, post};
use crate::common::Gw;
use crate::support::golden;

/// A signature in standard padded base64, as Google's are, with every
/// character that alphabet has and an id may not (`+`, `/`, `=`), so the
/// round trip is shown lossless.
pub(crate) const SIG: &str = "Eq0BCqoBAXLI2n+/sig/A+A=";

/// Whether `id` is one the Gemini egress minted: `call_`, a 12-hex-digit
/// random tag, a hex counter.
pub(crate) fn minted(id: &str) -> bool {
    id.strip_prefix("call_")
        .is_some_and(|t| t.len() > 12 && t.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Gemini's answer parts: some text, then two parallel calls, the first
/// signed (Gemini 3 signs the first call of a step only).
fn call_parts() -> Value {
    json!([
        {"text": "Checking."},
        {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}},
         "thoughtSignature": SIG},
        {"functionCall": {"name": "get_time", "args": {"city": "Paris"}}},
    ])
}

fn usage() -> Value {
    json!({"promptTokenCount": 9, "candidatesTokenCount": 4})
}

/// The non-streamed `generateContent` answer holding [`call_parts`].
pub(crate) fn calls_body() -> Value {
    json!({
        "modelVersion": "tgt-model",
        "candidates": [{"content": {"role": "model", "parts": call_parts()},
                        "finishReason": "STOP"}],
        "usageMetadata": usage(),
    })
}

/// The same answer streamed: the text, then the two calls, then the
/// empty-text last chunk that carries `finishReason`.
pub(crate) fn calls_sse() -> String {
    let parts = call_parts();
    let chunk = |parts: Value, last: bool| {
        let mut c = json!({"content": {"role": "model", "parts": parts}});
        if last {
            c["finishReason"] = json!("STOP");
        }
        format!(
            "data: {}\n\n",
            json!({"candidates": [c], "usageMetadata": usage()})
        )
    };
    [
        chunk(json!([parts[0]]), false),
        chunk(json!([parts[1], parts[2]]), false),
        chunk(json!([{"text": ""}]), true),
    ]
    .concat()
}

/// A plain text answer, non-streamed and streamed.
pub(crate) fn text_body(text: &str) -> Value {
    json!({
        "modelVersion": "tgt-model",
        "candidates": [{"content": {"role": "model", "parts": [{"text": text}]},
                        "finishReason": "STOP"}],
        "usageMetadata": usage(),
    })
}

pub(crate) fn text_sse(text: &str) -> String {
    format!("data: {}\n\n", text_body(text))
}

/// `replies` in order, one request each, streamed or not, on any
/// `generateContent` path.
pub(crate) async fn mount_replies(mock: &MockServer, replies: Vec<ResponseTemplate>) {
    for (i, reply) in replies.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path_regex(r"(?i)generatecontent$"))
            .respond_with(reply)
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(mock)
            .await;
    }
}

pub(crate) fn json_reply(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v)
}

pub(crate) fn sse_reply(s: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(s, "text/event-stream")
}

/// The bodies of the model requests `mock` received, in order.
pub(crate) async fn model_bodies(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            let p = r.url.path().to_ascii_lowercase();
            p.ends_with("generatecontent")
                || p.ends_with("/chat/completions")
                || p.ends_with("/messages")
        })
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// A gateway whose alias `m` is a Gemini model on `mock`.
pub(crate) async fn gemini_gateway(mock: &MockServer) -> Gw {
    gateway(mock, UpstreamKind::Generic, Protocol::Gemini)
        .await
        .1
}

/// Every tool call id in a client-facing answer — a JSON body, or the
/// data of every SSE event in a streamed one — in order, without repeats:
/// the strings under `id` or `call_id` that start with `call_` (a
/// completion's own id is `chatcmpl-…`, a Responses item's `fc_…`).
pub(crate) fn call_ids(answer: &str) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    if let (true, Some(s)) = (k == "id" || k == "call_id", x.as_str()) {
                        if s.starts_with("call_") && !out.iter().any(|o| o == s) {
                            out.push(s.to_string());
                        }
                    }
                    walk(x, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<Value>(answer) {
        walk(&v, &mut out);
        return out;
    }
    for line in answer.lines() {
        if let Some(d) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(d.trim()) {
                walk(&v, &mut out);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The three client shapes
// ---------------------------------------------------------------------------

/// One step of a conversation, as a client sends it back.
#[derive(Clone)]
pub(crate) enum Turn {
    User(&'static str),
    Assistant(&'static str),
    /// The model's calls: `(id, name)`, each with `{"city": "Paris"}`.
    Calls(Vec<(String, &'static str)>),
    /// Their results: `(id, text)`.
    Results(Vec<(String, &'static str)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ingress {
    ChatCompletions,
    Messages,
    Responses,
}

impl Ingress {
    pub(crate) const ALL: [Ingress; 3] = [
        Ingress::ChatCompletions,
        Ingress::Messages,
        Ingress::Responses,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Ingress::ChatCompletions => "chat_completions",
            Ingress::Messages => "messages",
            Ingress::Responses => "responses",
        }
    }

    fn route(self) -> &'static str {
        match self {
            Ingress::ChatCompletions => "/v1/chat/completions",
            Ingress::Messages => "/v1/messages",
            Ingress::Responses => "/v1/responses",
        }
    }

    fn tools(self) -> Value {
        let schema = json!({"type": "object", "properties": {"city": {"type": "string"}}});
        let tools = ["get_weather", "get_time"].map(|name| match self {
            Ingress::ChatCompletions => json!({"type": "function", "function": {
                "name": name, "description": name, "parameters": schema}}),
            Ingress::Messages => json!({"name": name, "description": name,
                                        "input_schema": schema}),
            Ingress::Responses => json!({"type": "function", "name": name,
                                         "description": name, "parameters": schema}),
        });
        json!(tools)
    }

    /// The request body for `history` in this shape.
    pub(crate) fn body(self, history: &[Turn], stream: bool) -> Value {
        let args = json!({"city": "Paris"});
        let mut items: Vec<Value> = Vec::new();
        for turn in history {
            match (self, turn) {
                (Ingress::Responses, Turn::User(t)) => {
                    items.push(json!({"role": "user", "content": t}))
                }
                (Ingress::Responses, Turn::Assistant(t)) => items.push(json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": t}],
                })),
                (_, Turn::User(t)) => items.push(json!({"role": "user", "content": t})),
                (_, Turn::Assistant(t)) => items.push(json!({"role": "assistant", "content": t})),
                (Ingress::ChatCompletions, Turn::Calls(calls)) => items.push(json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": calls.iter().map(|(id, name)| json!({
                        "id": id, "type": "function",
                        "function": {"name": name, "arguments": args.to_string()},
                    })).collect::<Vec<_>>(),
                })),
                (Ingress::ChatCompletions, Turn::Results(results)) => {
                    for (id, text) in results {
                        items.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
                    }
                }
                (Ingress::Messages, Turn::Calls(calls)) => items.push(json!({
                    "role": "assistant",
                    "content": calls.iter().map(|(id, name)| json!({
                        "type": "tool_use", "id": id, "name": name, "input": args,
                    })).collect::<Vec<_>>(),
                })),
                (Ingress::Messages, Turn::Results(results)) => items.push(json!({
                    "role": "user",
                    "content": results.iter().map(|(id, text)| json!({
                        "type": "tool_result", "tool_use_id": id, "content": text,
                    })).collect::<Vec<_>>(),
                })),
                (Ingress::Responses, Turn::Calls(calls)) => {
                    for (id, name) in calls {
                        items.push(json!({"type": "function_call", "call_id": id,
                                          "name": name, "arguments": args.to_string()}));
                    }
                }
                (Ingress::Responses, Turn::Results(results)) => {
                    for (id, text) in results {
                        items.push(json!({"type": "function_call_output", "call_id": id,
                                          "output": text}));
                    }
                }
            }
        }
        let mut body = json!({"model": "m", "tools": self.tools(), "stream": stream});
        match self {
            Ingress::Responses => body["input"] = json!(items),
            _ => body["messages"] = json!(items),
        }
        if self == Ingress::Messages {
            body["max_tokens"] = json!(256);
        }
        body
    }

    /// Send `history` through this shape; the answer's text.
    pub(crate) async fn send(self, gw: &Gw, history: &[Turn], stream: bool) -> String {
        let r = post(gw, self.route(), self.body(history, stream)).await;
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{}: {status} {text}", self.name());
        text
    }
}

/// Every `functionCall` part of a Gemini request body: its name and its
/// `thoughtSignature`, content by content.
pub(crate) fn signatures(body: &Value) -> Vec<Vec<(String, Option<String>)>> {
    body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            c["parts"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|p| {
                    let name = p.pointer("/functionCall/name")?.as_str()?.to_string();
                    Some((name, p["thoughtSignature"].as_str().map(String::from)))
                })
                .collect()
        })
        .collect()
}

/// Compare a Gemini request body with its fixture, without its system
/// instruction.
pub(crate) fn check(name: &str, body: &Value) {
    let mut body = body.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("systemInstruction");
    }
    if let Err(why) = golden::check("gemini_signatures", name, &golden::to_fixture(&body)) {
        panic!("{why}");
    }
}

#[test]
fn the_marker_is_part_of_the_protocol() {
    // The marker is what the fixtures' ids are read by; a rename is a
    // protocol change for every client holding an old id.
    assert_eq!(THOUGHT_SIGNATURE_MARKER, "__thoughtsig_");
}
