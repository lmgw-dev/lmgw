//! Anthropic answer blocks lmgw does not model are carried verbatim
//! (gateway design §6.1, changed 2026-11-05): `redacted_thinking` and a type
//! Anthropic adds next (`future_block` here, which no schema has) reach a
//! Messages client where they stood, whole and streamed, and go back to an
//! Anthropic upstream as they came. Every other upstream is sent none of
//! them, and every other client gets none: `x-lmgw-params-ignored` names
//! them (`content.<type>` for the request's, `answer.<type>` for the
//! answer's). The Chat's record keeps them for the replay and its thread
//! and exports show none.

use lmgw_core::agent::TurnAccumulator;
use lmgw_core::config::Protocol;
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::{AnthropicBlock, ContentPart, Message, Role, Signer};
use lmgw_core::sse::SseEvent;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::messages_context_management::{bodies_at, world};

const SIG: &str = "EqQBCkYIBxgCKkDsigned==";
const ENC: &str = "ENC-redacted-0a1b";

fn redacted() -> Value {
    json!({"type": "redacted_thinking", "data": ENC})
}

fn future() -> Value {
    json!({"type": "future_block", "note": "as it came", "payload": {"k": [1, 2.5, null]}})
}

fn content() -> Value {
    json!([
        redacted(),
        {"type": "thinking", "thinking": "", "signature": SIG},
        future(),
        {"type": "text", "text": "KIWI"},
    ])
}

fn answer() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
        "content": content(),
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 20},
    })
}

async fn anthropic_answering(answer: ResponseTemplate) -> (MockServer, crate::common::Gw) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(answer)
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    (mock, gw)
}

async fn post(gw: &crate::common::Gw, route: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}{route}"))
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .unwrap()
}

fn ignored(resp: &reqwest::Response) -> String {
    resp.headers()
        .get("x-lmgw-params-ignored")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_whole_answer_s_blocks_reach_the_messages_client_where_they_stood() {
    let (_mock, gw) = anthropic_answering(ResponseTemplate::new(200).set_body_json(answer())).await;
    let body = json!({"model": "m", "max_tokens": 64,
                      "messages": [{"role": "user", "content": "Code word?"}]});
    let resp = post(&gw, "/v1/messages", &body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), "", "nothing was left out");
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got["content"], content(), "{got}");
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// `redacted_thinking` whole in its start, a signed thinking block, a
/// `future_block` that streams a delta of its own, and the text.
fn stream_events() -> Vec<Value> {
    let start =
        |i: u64, b: Value| json!({"type": "content_block_start", "index": i, "content_block": b});
    let delta = |i: u64, d: Value| json!({"type": "content_block_delta", "index": i, "delta": d});
    let stop = |i: u64| json!({"type": "content_block_stop", "index": i});
    vec![
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt", "content": [],
            "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        start(0, redacted()),
        stop(0),
        start(
            1,
            json!({"type": "thinking", "thinking": "", "signature": ""}),
        ),
        delta(1, json!({"type": "signature_delta", "signature": SIG})),
        stop(1),
        start(
            2,
            json!({"type": "future_block", "note": "as ", "payload": {"k": [1, 2.5, null]}}),
        ),
        delta(2, json!({"type": "future_delta", "note": "it came"})),
        stop(2),
        start(3, json!({"type": "text", "text": ""})),
        delta(3, json!({"type": "text_delta", "text": "KIWI"})),
        stop(3),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": 20}}),
        json!({"type": "message_stop"}),
    ]
}

#[tokio::test]
async fn a_streamed_answer_s_blocks_reach_the_messages_client_as_they_streamed() {
    let (_mock, gw) = anthropic_answering(
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(&stream_events())),
    )
    .await;
    let body = json!({"model": "m", "max_tokens": 64, "stream": true,
                      "messages": [{"role": "user", "content": "Code word?"}]});
    let text = post(&gw, "/v1/messages", &body).await.text().await.unwrap();
    // Every block event as the upstream sent it, event name and JSON, at
    // the same index, in the same order.
    let frames: Vec<(String, Value)> = text
        .split("\n\n")
        .filter_map(|f| {
            let name = f.lines().find_map(|l| l.strip_prefix("event: "))?;
            let data = f.lines().find_map(|l| l.strip_prefix("data: "))?;
            Some((name.to_string(), serde_json::from_str(data).ok()?))
        })
        .filter(|(name, _)| name.starts_with("content_block"))
        .collect();
    let upstream: Vec<(String, Value)> = stream_events()
        .into_iter()
        .filter(|e| e["type"].as_str().unwrap().starts_with("content_block"))
        .map(|e| (e["type"].as_str().unwrap().to_string(), e))
        .collect();
    assert_eq!(frames, upstream, "{text}");
}

/// The in-process assembler (the agent loop's, the Chat's tool turns')
/// records each block whole, its deltas folded in, where it stood, with
/// the origin that answered.
#[test]
fn the_assembler_records_a_streamed_block_whole() {
    let mut dec = for_protocol(Protocol::Anthropic).new_decoder();
    let mut acc = TurnAccumulator::default();
    for e in stream_events() {
        for d in dec.on_event(&SseEvent {
            event: Some(e["type"].as_str().unwrap().into()),
            data: e.to_string(),
        }) {
            acc.on_delta(&d);
        }
    }
    let c = acc.finish("tgt".into());
    let row = Signer::default();
    assert_eq!(
        c.content,
        [
            ContentPart::AnthropicBlock(AnthropicBlock::answered(redacted(), &row)),
            ContentPart::signed_reasoning("", SIG, Some(row.clone())),
            ContentPart::AnthropicBlock(AnthropicBlock::answered(future(), &row)),
            ContentPart::text("KIWI"),
        ]
    );
}

/// A verbatim block closes when the upstream's `content_block_stop` came,
/// not at the next block's start: a `server_tool_use` block stays open
/// while the provider's search runs, and the Messages client learns its
/// call is whole from that stop.
#[test]
fn a_verbatim_block_closes_when_the_upstream_s_stop_comes() {
    use lmgw_core::ingress::anthropic::AnthropicStreamEncoder;
    use lmgw_core::ingress::ClientStreamEncoder;
    let mut dec = for_protocol(Protocol::Anthropic).new_decoder();
    let mut enc = AnthropicStreamEncoder::new("m");
    let events = [
        json!({"type": "content_block_start", "index": 0, "content_block":
               {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "input_json_delta", "partial_json": "{\"query\": \"kiwi\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
    ];
    let out: String = events
        .iter()
        .flat_map(|e| {
            dec.on_event(&SseEvent {
                event: Some(e["type"].as_str().unwrap().into()),
                data: e.to_string(),
            })
        })
        .map(|d| enc.delta(&d))
        .collect();
    let names: Vec<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .collect();
    assert_eq!(
        names,
        [
            "content_block_start",
            "content_block_delta",
            "content_block_stop"
        ],
        "{out}"
    );
    // The answer's end writes no second stop for it.
    let end = enc.finish();
    assert!(!end.contains("content_block_stop"), "{end}");
}

#[tokio::test]
async fn blocks_sent_back_reach_anthropic_verbatim() {
    let (mock, gw) = anthropic_answering(ResponseTemplate::new(200).set_body_json(answer())).await;
    let body = json!({"model": "m", "max_tokens": 64, "messages": [
        {"role": "user", "content": "Code word?"},
        {"role": "assistant", "content": content()},
        {"role": "user", "content": [future(), {"type": "text", "text": "Thanks."}]},
    ]});
    let resp = post(&gw, "/v1/messages", &body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), "");
    let sent = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(sent[0]["messages"][1]["content"], content(), "{}", sent[0]);
    assert_eq!(sent[0]["messages"][2]["content"][0], future());
}

#[tokio::test]
async fn another_upstream_is_sent_none_and_the_answer_names_them() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let body = json!({"model": "m", "max_tokens": 64, "messages": [
        {"role": "user", "content": "Code word?"},
        {"role": "assistant", "content": content()},
        {"role": "user", "content": "Thanks."},
    ]});
    let resp = post(&gw, "/v1/messages", &body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        ignored(&resp),
        "content.redacted_thinking,content.future_block"
    );
    let sent = bodies_at(&mock, "/chat/completions").await;
    let text = sent[0].to_string();
    for leaked in [ENC, "future_block", "as it came", SIG] {
        assert!(!text.contains(leaked), "{leaked}: {text}");
    }
    assert_eq!(sent[0]["messages"][1]["content"], "KIWI");
}

/// An image and a document whose source is a Files API reference name what
/// only Anthropic holds: an Anthropic row gets both as written, any other
/// row neither, and the response names them.
#[tokio::test]
async fn a_file_source_image_and_document_go_to_anthropic_only() {
    let image = json!({"type": "image", "source": {"type": "file", "file_id": "file_img"}});
    let document = json!({"type": "document", "source": {"type": "file", "file_id": "file_pdf"}});
    let body = json!({"model": "m", "max_tokens": 64, "messages": [
        {"role": "user", "content": [image, document, {"type": "text", "text": "What is it?"}]},
    ]});

    let (mock, gw) = anthropic_answering(ResponseTemplate::new(200).set_body_json(answer())).await;
    let resp = post(&gw, "/v1/messages", &body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), "");
    let sent = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(sent[0]["messages"][0]["content"][0], image);
    assert_eq!(sent[0]["messages"][0]["content"][1], document);

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let resp = post(&gw, "/v1/messages", &body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), "content.image,content.document");
    let text = bodies_at(&mock, "/chat/completions").await[0].to_string();
    assert!(
        !text.contains("file_img") && !text.contains("file_pdf"),
        "{text}"
    );
    assert!(text.contains("What is it?"), "{text}");
}

/// A Chat Completions or Responses client gets the answer without the
/// blocks its wire has no place for, and the response names them.
#[tokio::test]
async fn chat_and_responses_clients_get_none_and_the_answer_names_them() {
    let (_mock, gw) = anthropic_answering(ResponseTemplate::new(200).set_body_json(answer())).await;
    for (route, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "m", "messages": [{"role": "user", "content": "Code word?"}]}),
        ),
        (
            "/v1/responses",
            json!({"model": "m", "input": "Code word?", "store": false}),
        ),
    ] {
        let resp = post(&gw, route, &body).await;
        assert_eq!(resp.status(), 200, "{route}");
        assert_eq!(
            ignored(&resp),
            "answer.redacted_thinking,answer.future_block",
            "{route}"
        );
        let text = resp.text().await.unwrap();
        for leaked in [ENC, "future_block", "as it came", SIG] {
            assert!(!text.contains(leaked), "{route}: {leaked}: {text}");
        }
        assert!(text.contains("KIWI"), "{route}: {text}");
    }
}

#[tokio::test]
async fn a_block_without_a_type_is_refused() {
    let (mock, gw) = anthropic_answering(ResponseTemplate::new(200).set_body_json(answer())).await;
    let body = json!({"model": "m", "max_tokens": 64, "messages": [
        {"role": "user", "content": [{"text": "no type"}]},
    ]});
    let resp = post(&gw, "/v1/messages", &body).await;
    assert_eq!(resp.status(), 400);
    assert!(bodies_at(&mock, "/v1/messages").await.is_empty());
}

/// The Chat's tool-turn record keeps an Anthropic answer's signature and
/// its verbatim blocks for the next turn's replay; the thread and both
/// exports show the thinking text and neither.
#[tokio::test]
async fn the_thread_and_both_exports_show_a_record_without_them() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw = crate::common::serve(state.clone()).await;
    let row = Signer::new("https://api.anthropic.com");
    let record = vec![
        Message::text(Role::User, "Code word?"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::AnthropicBlock(AnthropicBlock::answered(redacted(), &row)),
                ContentPart::signed_reasoning("Look it up.", SIG, Some(row.clone())),
                ContentPart::AnthropicBlock(AnthropicBlock::answered(future(), &row)),
                ContentPart::ToolUse {
                    id: "toolu_1".into(),
                    name: "lookup".into(),
                    args: json!({}),
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                id: "toolu_1".into(),
                name: Some("lookup".into()),
                content: lmgw_core::ir::ToolResultBlock::one("KIWI"),
                is_error: false,
            }],
        },
    ];
    let tid = crate::chat_golden::thread(&gw, json!({"model_alias": "m"})).await;
    store::append_chat_message(&state.db, tid, "user", "Code word?", "", None, None, None)
        .await
        .unwrap();
    store::append_chat_reply(
        &state.db,
        tid,
        &store::ChatReply {
            content: "KIWI".into(),
            ir_messages: Some(serde_json::to_string(&record).unwrap()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let get = |route: String| {
        let client = gw.client();
        let url = format!("{gw}{route}");
        async move { client.get(url).send().await.unwrap().text().await.unwrap() }
    };
    for (what, body) in [
        ("the thread", get(format!("/chat/api/threads/{tid}")).await),
        (
            "the JSON export",
            get(format!("/chat/api/threads/{tid}/export?format=json")).await,
        ),
        (
            "the Markdown export",
            get(format!("/chat/api/threads/{tid}/export?format=md")).await,
        ),
    ] {
        assert!(body.contains("Look it up."), "{what}: {body}");
        for leaked in [SIG, ENC, "future_block", "anthropic_block", "cfg9"] {
            assert!(!body.contains(leaked), "{what}: {leaked}: {body}");
        }
    }
    // The store keeps them for the replay.
    let kept = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap()
        .ir_messages
        .unwrap();
    assert!(kept.contains(SIG) && kept.contains(ENC) && kept.contains("future_block"));
}
