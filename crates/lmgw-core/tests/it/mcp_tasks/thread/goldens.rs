//! The request a late result goes out in (MCP Tasks design §3.2, §7), byte
//! for byte, on an OpenAI-shaped, an Anthropic-shaped, a Gemini and a
//! llama-server upstream (Gemini's synthetic call carrying the documented
//! `thoughtSignature` for a call the model did not make, as does any
//! current-turn step's first call that has no signature of its own;
//! gateway design §7.1). Each pair renders where its result is stored
//! (chronological order, design T11): a send after the result entered (the
//! pair joined to the reply that started the job, the new message after
//! it), a send that finds the result still waiting (it enters before the
//! message, in the message's own write: the same bytes), and an `answer`
//! (the call joined to the reply, the result last). The thread's history is stored as a tool turn leaves it, and its
//! task row as the follower ends it.
//! `tests/fixtures/mcp_tasks/<case>__<upstream>.json`; `LMGW_BLESS=1`
//! rewrites them (`support/golden.rs`).

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::{ContentPart, Message, Role, ToolResultBlock};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, mcp_tasks};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, openai_sse, post};
use crate::common::Gw;
use crate::support::golden;

fn anthropic_sse(text: &str) -> String {
    format!(
        "event: message_start\ndata: {}\n\n\
         event: content_block_start\ndata: {}\n\n\
         event: content_block_delta\ndata: {}\n\n\
         event: message_delta\ndata: {}\n\n\
         event: message_stop\ndata: {}\n\n",
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 7}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
               "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    )
}

fn gemini_sse(text: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]},
                               "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3}})
    )
}

/// The four upstreams: a name for the fixture, and the row's spelling.
const UPSTREAMS: [(&str, UpstreamKind, Protocol); 4] = [
    ("openai", UpstreamKind::Generic, Protocol::Openai),
    ("anthropic", UpstreamKind::Generic, Protocol::Anthropic),
    ("gemini", UpstreamKind::Generic, Protocol::Gemini),
    ("llama", UpstreamKind::LlamaServer, Protocol::LlamaCpp),
];

/// Whether `protocol`'s wire is OpenAI-shaped (`assert_strict` reads it).
fn openai_shaped(protocol: Protocol) -> bool {
    matches!(protocol, Protocol::Openai | Protocol::LlamaCpp)
}

/// A gateway on `protocol` with a thread holding `build it`, the tool turn
/// that started job `t1` and said `Started.`, and the job ended with
/// `42 files`, its result waiting: the mock, the gateway and the thread.
async fn seeded(kind: UpstreamKind, protocol: Protocol) -> (MockServer, SharedState, Gw, i64) {
    let mock = MockServer::start().await;
    let reply = match protocol {
        Protocol::Anthropic => anthropic_sse("Built."),
        Protocol::Gemini => gemini_sse("Built."),
        _ => openai_sse("Built.", 7, 3),
    };
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(reply, "text/event-stream"))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, kind, protocol).await;
    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "m"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let record = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse {
                id: "call_1".into(),
                name: "desktop__build".into(),
                args: json!({}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                id: "call_1".into(),
                name: Some("desktop__build".into()),
                content: ToolResultBlock::one("started, job t1"),
                is_error: false,
            }],
        },
    ];
    let record = serde_json::to_string(&record).unwrap();
    store::append_chat_message(&state.db, tid, "user", "build it", "", None, None, None)
        .await
        .unwrap();
    store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "Started.",
        "",
        None,
        None,
        Some(&record),
    )
    .await
    .unwrap();
    let id = mcp_tasks::insert(
        &state.db,
        &mcp_tasks::NewMcpTask {
            server_id: 9,
            server_label: "desktop",
            task_id: "t1",
            thread_id: tid,
            tool: "desktop__build",
            call_id: "call_1",
            started_by: None,
            status: "working",
            status_message: None,
            poll_interval_ms: None,
            ttl_ms: None,
        },
    )
    .await
    .unwrap();
    let result = serde_json::to_string(&ToolResultBlock::one(
        "job t1 (desktop__build) completed\n42 files",
    ))
    .unwrap();
    let ended = mcp_tasks::Ended {
        status: "completed",
        status_message: None,
        result: &result,
        ended_by: None,
    };
    assert!(mcp_tasks::end(&state.db, id, &ended).await.unwrap());
    (mock, state, gw, tid)
}

/// The body of the last model request `mock` received.
async fn last_body(mock: &MockServer) -> Value {
    let reqs = mock.received_requests().await.unwrap();
    let r = reqs
        .iter()
        .rfind(|r| {
            let p = r.url.path();
            p.ends_with("/chat/completions")
                || p.ends_with("/messages")
                || p.ends_with(":streamGenerateContent")
        })
        .expect("a model request");
    serde_json::from_slice(&r.body).unwrap()
}

/// Compare `body` with its fixture, its system prompt left out: it names
/// today's date, and the history after it is what the case is about.
fn check(name: &str, body: &Value) {
    let mut body = body.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("system");
        o.remove("systemInstruction");
    }
    if let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) {
        msgs.retain(|m| m["role"] != "system");
    }
    if let Err(why) = golden::check("mcp_tasks", name, &golden::to_fixture(&body)) {
        panic!("{why}");
    }
}

/// A send after the result entered the idle thread, on each upstream: the
/// pair joined to the reply before it, the new user message after the
/// result (one user turn with it on Anthropic and Gemini), and the wire
/// check on the OpenAI-shaped wires.
#[tokio::test]
async fn a_send_after_a_result_on_each_upstream() {
    send_case("send", true).await;
}

/// A send that finds the result still waiting (design §3.1's third
/// moment), on each upstream: the result enters before the send's message,
/// so the request is byte for byte the one of a send after the result
/// entered — the model answers the message, the result before it.
#[tokio::test]
async fn a_send_that_lets_a_result_in_on_each_upstream() {
    send_case("send", false).await;
}

/// The send cases: the result entered before the send (`entered`), or
/// waits for the send to let it in.
async fn send_case(case: &str, entered: bool) {
    for (name, kind, protocol) in UPSTREAMS {
        let (mock, state, gw, tid) = seeded(kind, protocol).await;
        if entered {
            assert_eq!(state.deliver_chat_tasks_for_tests(tid).await, 1, "{name}");
        }
        let body = post(
            &gw,
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "and?"}),
        )
        .await
        .text()
        .await
        .unwrap();
        assert!(body.contains("event: done"), "{name}: {body}");
        let sent = last_body(&mock).await;
        if openai_shaped(protocol) {
            super::assert_strict(sent["messages"].as_array().unwrap());
        }
        if protocol == Protocol::Gemini {
            // The stored call is a turn back and has no signature: the skip
            // value all the same (gateway design §7.1, every step).
            gemini_signs(
                &sent,
                &[
                    ("desktop__build", Some(SKIP)),
                    ("lmgw__job_result", Some(SKIP)),
                ],
            );
        }
        check(&format!("{case}__{name}"), &sent);
    }
}

/// `answer` on each upstream: the call joined to the reply that started the
/// job, the result last.
#[tokio::test]
async fn an_answer_on_each_upstream() {
    for (name, kind, protocol) in UPSTREAMS {
        let (mock, _state, gw, tid) = seeded(kind, protocol).await;
        let body = post(&gw, &format!("/chat/api/threads/{tid}/answer"), json!({}))
            .await
            .text()
            .await
            .unwrap();
        assert!(body.contains("event: done"), "{name}: {body}");
        let sent = last_body(&mock).await;
        if openai_shaped(protocol) {
            super::assert_strict(sent["messages"].as_array().unwrap());
        }
        if protocol == Protocol::Gemini {
            // The call that started the job has no signature in its record
            // (written by an OpenAI-shaped mock).
            gemini_signs(
                &sent,
                &[
                    ("desktop__build", Some(SKIP)),
                    ("lmgw__job_result", Some(SKIP)),
                ],
            );
        }
        check(&format!("answer__{name}"), &sent);
    }
}

/// The value Google documents for a call the model did not make.
const SKIP: &str = "skip_thought_signature_validator";

/// On the Gemini wire the calls, in order, carry `want`'s
/// `thoughtSignature`s.
fn gemini_signs(body: &Value, want: &[(&str, Option<&str>)]) {
    let calls: Vec<(&str, Option<&str>)> = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["parts"].as_array().unwrap())
        .filter_map(|p| {
            Some((
                p.pointer("/functionCall/name")?.as_str()?,
                p.get("thoughtSignature").and_then(Value::as_str),
            ))
        })
        .collect();
    assert_eq!(calls, want, "{body:#}");
}
