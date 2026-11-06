//! Where an Anthropic signature (and a verbatim block) may go (gateway
//! design §6.1, "Where a signature goes", changed 2026-11-06). One lmgw
//! read from an answer names the origin that answered ([`Signer`]), and
//! lmgw's own records (the agent loop's turns, the Chat's tool turns, a
//! stored Responses chain) send it to every Anthropic-protocol row at that
//! origin, whatever its key, its headers or the model, as Anthropic asks
//! (it drops a block the model asked cannot read); a row at another origin
//! gets the turn without it, logged and named in `x-lmgw-params-ignored`.
//! A client's own block names no origin and goes as the client wrote it.
//! No other egress is ever sent a signature, an empty signed block
//! included; their readers still get the thinking text, as reasoning.

use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::*;
use lmgw_core::sse::SseEvent;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::messages_context_management::world;

const SIG_A: &str = "EqQBCkYIBxgCKkDomitted==";
const SIG_B: &str = "EqQBCkYIBxgCKkDsummary==";

fn row(id: i64, protocol: Protocol) -> Upstream {
    at(id, protocol, "http://h:1/v1")
}

fn at(id: i64, protocol: Protocol, base_url: &str) -> Upstream {
    Upstream {
        id,
        name: format!("row-{id}"),
        protocol,
        kind: UpstreamKind::Generic,
        base_url: base_url.into(),
        api_key: Some("sk-test".into()),
        extra_headers: vec![],
        timeout_ms: 1000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn request(assistant: Vec<ContentPart>) -> ChatRequest {
    ChatRequest {
        model_alias: "m".into(),
        messages: vec![
            Message::text(Role::User, "What was the code word?"),
            Message {
                role: Role::Assistant,
                content: assistant,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    id: "toolu_1".into(),
                    name: Some("lookup".into()),
                    content: ToolResultBlock::one("KIWI"),
                    is_error: false,
                }],
            },
        ],
        params: Params::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        response_format: None,
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
        context_management: None,
        compaction: None,
        thinking_display: None,
    }
}

fn call() -> ContentPart {
    ContentPart::ToolUse {
        id: "toolu_1".into(),
        name: "lookup".into(),
        args: json!({"q": "kiwi"}),
    }
}

fn body(up: &Upstream, ir: &ChatRequest) -> Value {
    let req = for_protocol(up.protocol)
        .build_chat(
            &reqwest::Client::new(),
            up,
            "tgt",
            ir,
            &Params::default(),
            false,
        )
        .unwrap()
        .build()
        .unwrap();
    serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
}

/// The assistant turn's blocks as an Anthropic row is sent them.
fn assistant_blocks(up: &Upstream, ir: &ChatRequest) -> Value {
    body(up, ir)["messages"][1]["content"].clone()
}

#[test]
fn a_recorded_signature_goes_to_every_row_at_its_origin_only() {
    let one = row(1, Protocol::Anthropic);
    let scope = Signer::of(&one);
    let ir = request(vec![
        ContentPart::signed_reasoning("Look it up.", SIG_A, Some(scope.clone())),
        ContentPart::AnthropicBlock(AnthropicBlock::answered(
            json!({"type": "redacted_thinking", "data": "ENC"}),
            &scope,
        )),
        call(),
    ]);
    let whole = json!([
        {"type": "thinking", "thinking": "Look it up.", "signature": SIG_A},
        {"type": "redacted_thinking", "data": "ENC"},
        {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "kiwi"}},
    ]);
    // The row, another row there (a fallback, a second account), the row
    // given another key or a header since: Anthropic reads or drops it.
    let rekeyed = Upstream {
        api_key: Some("sk-other".into()),
        extra_headers: vec![("anthropic-beta".into(), "x".into())],
        ..one.clone()
    };
    for up in [&one, &row(2, Protocol::Anthropic), &rekeyed] {
        assert_eq!(assistant_blocks(up, &ir), whole, "{}", up.name);
        assert!(ir.blocks_ignored(up).is_empty(), "{}", up.name);
    }
    // A row at another origin: the turn without them, and named.
    let elsewhere = at(3, Protocol::Anthropic, "https://api.example.com/anthropic");
    assert_eq!(
        assistant_blocks(&elsewhere, &ir),
        json!([
            {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "kiwi"}},
        ])
    );
    assert_eq!(
        ir.blocks_ignored(&elsewhere),
        ["content.thinking", "content.redacted_thinking"]
    );
}

#[test]
fn a_client_s_signature_goes_to_every_anthropic_row_as_written() {
    let ir = request(vec![
        ContentPart::signed_reasoning("Look it up.", SIG_A, None),
        ContentPart::AnthropicBlock(AnthropicBlock::from_client(
            json!({"type": "redacted_thinking", "data": "ENC"}),
        )),
        call(),
    ]);
    for id in [1, 2] {
        let blocks = assistant_blocks(&row(id, Protocol::Anthropic), &ir);
        assert_eq!(blocks[0]["signature"], SIG_A, "row {id}");
        assert_eq!(blocks[1]["data"], "ENC", "row {id}");
    }
}

/// Every send site reads an answer for its row: the whole one and the
/// streamed one name its origin on every signature and verbatim block.
#[test]
fn an_answer_names_the_origin_that_answered() {
    let up = row(7, Protocol::Anthropic);
    let scope = Some(Signer::of(&up));
    let egress = for_protocol(Protocol::Anthropic);
    let whole = json!({
        "content": [
            {"type": "thinking", "thinking": "", "signature": SIG_A},
            {"type": "redacted_thinking", "data": "ENC"},
            {"type": "text", "text": "KIWI"},
        ],
        "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 2},
    });
    let c = egress
        .parse_completion_from(&up, whole.to_string().as_bytes())
        .unwrap();
    assert_eq!(
        c.content[0],
        ContentPart::signed_reasoning("", SIG_A, scope.clone())
    );
    assert_eq!(
        c.content[1],
        ContentPart::AnthropicBlock(AnthropicBlock::answered(
            json!({"type": "redacted_thinking", "data": "ENC"}),
            &Signer::of(&up),
        ))
    );

    let mut dec = egress.decoder_for(&up);
    let events = [
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "signature_delta", "signature": SIG_B}}),
    ];
    let deltas: Vec<StreamDelta> = events
        .iter()
        .flat_map(|e| {
            dec.on_event(&SseEvent {
                event: Some(e["type"].as_str().unwrap().into()),
                data: e.to_string(),
            })
        })
        .collect();
    assert_eq!(
        deltas,
        [StreamDelta::ReasoningSignature {
            signature: SIG_B.into(),
            scope: Signer::of(&up),
        }]
    );
}

/// No OpenAI-shaped, Gemini or llama.cpp body carries an Anthropic
/// signature, an omitted display's empty block included; the compatible
/// and llama.cpp rows get the signed block's text as `reasoning_content`,
/// with nothing added for the empty one.
#[test]
fn a_signed_part_never_reaches_another_egress() {
    let scope = Signer::of(&row(1, Protocol::Anthropic));
    let ir = request(vec![
        ContentPart::signed_reasoning("", SIG_A, Some(scope.clone())),
        ContentPart::signed_reasoning("The word was KIWI.", SIG_B, Some(scope)),
        call(),
    ]);
    for protocol in [
        Protocol::Openai,
        Protocol::OpenaiCompatible,
        Protocol::Gemini,
        Protocol::LlamaCpp,
    ] {
        let mut up = row(5, protocol);
        if protocol == Protocol::LlamaCpp {
            up.kind = UpstreamKind::LlamaServer;
        }
        let sent = body(&up, &ir);
        let text = sent.to_string();
        for sig in [SIG_A, SIG_B] {
            assert!(!text.contains(sig), "{protocol:?}: {text}");
        }
        if matches!(protocol, Protocol::OpenaiCompatible | Protocol::LlamaCpp) {
            assert_eq!(
                sent["messages"][1]["reasoning_content"], "The word was KIWI.",
                "{protocol:?}: {text}"
            );
        }
    }
}

/// A signed answer's thinking still reaches the other wires as reasoning:
/// Chat Completions' `reasoning_content` and a Responses reasoning item,
/// the empty omitted block adding nothing and no signature anywhere.
#[tokio::test]
async fn chat_and_responses_clients_still_get_a_signed_answer_s_reasoning() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [
                {"type": "thinking", "thinking": "", "signature": SIG_A},
                {"type": "thinking", "thinking": "The word was KIWI.", "signature": SIG_B},
                {"type": "text", "text": "KIWI"},
            ],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 100, "output_tokens": 89},
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let client = reqwest::Client::new();

    let chat = client
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "Code word?"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 200);
    let text = chat.text().await.unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let msg = &v["choices"][0]["message"];
    assert_eq!(msg["reasoning_content"], "The word was KIWI.", "{text}");
    assert_eq!(msg["content"], "KIWI");
    assert!(!text.contains(SIG_A) && !text.contains(SIG_B), "{text}");

    let responses = client
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "m", "input": "Code word?", "store": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(responses.status(), 200);
    let text = responses.text().await.unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let reasoning: Vec<&Value> = v["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "reasoning")
        .collect();
    assert_eq!(reasoning.len(), 1, "{text}");
    assert_eq!(
        reasoning[0]["summary"][0]["text"], "The word was KIWI.",
        "{text}"
    );
    assert!(!text.contains(SIG_A) && !text.contains(SIG_B), "{text}");
}

/// A recorded tool loop replayed to a row at another origin (a fallback at
/// another provider) loses the thinking that led its assistant turn, and
/// still goes out with thinking as asked: Anthropic accepts a tool loop
/// without that block (probed 2026-11-06: Claude Haiku 5.5 and Claude
/// Sonnet 5.5 adaptive, Claude Haiku 4.5 with a manual budget, which runs
/// the turn with thinking off), while thinking `disabled` is refused by
/// Claude Opus 5.5 and Claude Sonnet 5.5. The send says so in one INFO line.
#[tokio::test]
async fn a_trailing_tool_loop_across_origins_goes_out_with_thinking_and_says_so() {
    let (log, _guard) = crate::common::captured_log::capture_log();
    let here = row(1, Protocol::Anthropic);
    let elsewhere = at(3, Protocol::Anthropic, "https://api.example.com/anthropic");
    let ir = request(vec![
        ContentPart::signed_reasoning("Look it up.", SIG_A, Some(Signer::of(&here))),
        call(),
    ]);
    let params = Params {
        reasoning: Some(ReasoningControl {
            effort: Some("high".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let req = for_protocol(Protocol::Anthropic)
        .build_chat(
            &reqwest::Client::new(),
            &elsewhere,
            "tgt",
            &ir,
            &params,
            false,
        )
        .unwrap()
        .build()
        .unwrap();
    let sent: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(sent["thinking"], json!({"type": "adaptive"}));
    assert_eq!(sent["output_config"]["effort"], "high");
    assert_eq!(sent["messages"][1]["content"][0]["type"], "tool_use");
    let text = log.text();
    assert!(text.contains("INFO"), "{text}");
    assert!(text.contains("trailing tool loop"), "{text}");
    assert!(text.contains("signed_at=http://h:1"), "{text}");
    assert!(text.contains("row=https://api.example.com"), "{text}");
}
