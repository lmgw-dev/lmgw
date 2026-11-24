//! The prompt-caching marks through the egresses: an Anthropic row is sent
//! each mark on the block the client put it on, and every other egress
//! builds the body it builds for the same request without them.

use serde_json::{json, Value};

use crate::config::{Protocol, Upstream, UpstreamKind};
use crate::egress::for_protocol;
use crate::ingress::anthropic::parse_messages_request;
use crate::ingress::openai::parse_chat_request;
use crate::ir::ChatRequest;

const PNG: &str = "iVBORw0KGgo=";

fn upstream(protocol: Protocol) -> Upstream {
    Upstream {
        id: 7,
        name: "row".into(),
        protocol,
        kind: match protocol {
            Protocol::LlamaCpp => UpstreamKind::LlamaServer,
            _ => UpstreamKind::Generic,
        },
        base_url: "http://upstream.test/v1".into(),
        api_key: Some("sk-test".into()),
        extra_headers: Vec::new(),
        timeout_ms: 1000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

/// The body `protocol`'s egress posts for `ir`.
fn sent(protocol: Protocol, ir: &ChatRequest) -> Value {
    let req = for_protocol(protocol)
        .build_chat(
            &reqwest::Client::new(),
            &upstream(protocol),
            "m",
            ir,
            &ir.params,
            false,
        )
        .unwrap()
        .build()
        .unwrap();
    serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
}

/// A Messages body with a mark at every placement and a tool with other
/// Anthropic fields.
fn marked() -> Value {
    let five = json!({"type": "ephemeral"});
    let hour = json!({"type": "ephemeral", "ttl": "1h"});
    json!({
        "model": "m",
        "max_tokens": 64,
        "cache_control": five,
        "system": [
            {"type": "text", "text": "You are terse."},
            {"type": "text", "text": "Here is the manual.", "cache_control": hour},
        ],
        "tools": [{
            "name": "lookup",
            "description": "Looks a word up.",
            "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
            "cache_control": five,
            "eager_input_streaming": true,
            "input_examples": [{"q": "cat"}],
        }],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "Look at this.", "cache_control": five},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG},
                 "cache_control": five},
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Looking."},
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "cat"},
                 "cache_control": five},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "cache_control": hour, "content": [
                    {"type": "text", "text": "a small animal", "cache_control": five},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG},
                     "cache_control": five},
                ]},
                {"type": "text", "text": "And now?"},
            ]},
        ],
    })
}

/// [`marked`] without any `cache_control` and without the tool's other
/// Anthropic fields: the same request from a client that never marked it.
fn unmarked() -> Value {
    fn strip(v: &mut Value) {
        match v {
            Value::Array(a) => a.iter_mut().for_each(strip),
            Value::Object(o) => {
                o.remove("cache_control");
                o.values_mut().for_each(strip);
            }
            _ => {}
        }
    }
    let mut v = marked();
    strip(&mut v);
    for t in v["tools"].as_array_mut().unwrap() {
        let t = t.as_object_mut().unwrap();
        t.remove("eager_input_streaming");
        t.remove("input_examples");
    }
    v
}

#[test]
fn an_anthropic_row_is_sent_every_mark_on_its_own_block() {
    let client = marked();
    let body = sent(
        Protocol::Anthropic,
        &parse_messages_request(&client).unwrap(),
    );
    assert_eq!(body["cache_control"], client["cache_control"]);
    // The system blocks stay blocks, each with its own mark (or none).
    assert_eq!(body["system"], client["system"]);
    // The tool comes back with every field it was sent with.
    assert_eq!(body["tools"], client["tools"]);
    // Every message block as the client wrote it, marks where they were:
    // the IR's tool turn and the user's text after it merge back into one
    // user turn, each block keeping its own.
    assert_eq!(body["messages"], client["messages"]);
}

#[test]
fn a_string_system_stays_a_string_and_one_unmarked_block_becomes_one() {
    let string = json!({
        "model": "m", "max_tokens": 8, "system": "Be brief.",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let body = sent(
        Protocol::Anthropic,
        &parse_messages_request(&string).unwrap(),
    );
    assert_eq!(body["system"], json!("Be brief."));
    assert!(body.get("cache_control").is_none());

    // One block with no mark has nothing a string would lose.
    let mut one = string.clone();
    one["system"] = json!([{"type": "text", "text": "Be brief."}]);
    let body = sent(Protocol::Anthropic, &parse_messages_request(&one).unwrap());
    assert_eq!(body["system"], json!("Be brief."));

    // A `null` mark is no mark.
    one["system"] = json!([{"type": "text", "text": "Be brief.", "cache_control": null}]);
    one["cache_control"] = Value::Null;
    let body = sent(Protocol::Anthropic, &parse_messages_request(&one).unwrap());
    assert_eq!(body["system"], json!("Be brief."));
    assert!(body.get("cache_control").is_none());
}

/// A tool result as a Messages client writes it, `blocks` as its content.
fn tool_result_request(blocks: Value) -> Value {
    json!({
        "model": "m", "max_tokens": 8,
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "t", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": blocks},
            ]},
        ],
    })
}

#[test]
fn every_block_of_a_tool_result_is_a_block_of_its_own_with_its_own_mark() {
    let five = json!({"type": "ephemeral"});
    let client = tool_result_request(json!([
        {"type": "text", "text": "one"},
        {"type": "text", "text": "two", "cache_control": five},
        {"type": "text", "text": "three"},
        {"type": "text", "text": ""},
    ]));
    let body = sent(
        Protocol::Anthropic,
        &parse_messages_request(&client).unwrap(),
    );
    assert_eq!(
        body["messages"][2]["content"][0]["content"],
        json!([
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two", "cache_control": five},
            {"type": "text", "text": "three"},
        ])
    );
}

#[test]
fn a_tool_result_is_sent_the_same_wherever_the_mark_sits() {
    let five = json!({"type": "ephemeral"});
    let with = tool_result_request(json!([
        {"type": "text", "text": "a", "cache_control": five},
        {"type": "text", "text": "b"},
    ]));
    let without = tool_result_request(json!([
        {"type": "text", "text": "a"},
        {"type": "text", "text": "b"},
    ]));
    let mut marked = sent(Protocol::Anthropic, &parse_messages_request(&with).unwrap());
    let unmarked = sent(
        Protocol::Anthropic,
        &parse_messages_request(&without).unwrap(),
    );
    assert_eq!(
        marked["messages"][2]["content"][0]["content"][0]["cache_control"],
        five
    );
    marked["messages"][2]["content"][0]["content"][0]
        .as_object_mut()
        .unwrap()
        .remove("cache_control");
    assert_eq!(marked, unmarked);
}

#[test]
fn a_verbatim_block_keeps_its_own_mark_for_an_anthropic_row() {
    let five = json!({"type": "ephemeral"});
    let document = json!({
        "type": "document",
        "source": {"type": "text", "media_type": "text/plain", "data": "the manual"},
        "cache_control": five,
    });
    let compaction = json!({
        "type": "compaction", "content": "earlier, briefly", "cache_control": five,
    });
    let client = json!({
        "model": "m", "max_tokens": 8,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "go"}]},
            {"role": "assistant", "content": [
                compaction,
                {"type": "tool_use", "id": "toolu_1", "name": "t", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": [document]},
                document,
            ]},
        ],
    });
    let body = sent(
        Protocol::Anthropic,
        &parse_messages_request(&client).unwrap(),
    );
    assert_eq!(body["messages"][1]["content"][0], compaction);
    assert_eq!(body["messages"][2]["content"][0]["content"][0], document);
    assert_eq!(body["messages"][2]["content"][1], document);
}

#[test]
fn every_other_egress_builds_the_body_it_builds_without_the_marks() {
    let with = parse_messages_request(&marked()).unwrap();
    let without = parse_messages_request(&unmarked()).unwrap();
    assert_ne!(with, without);
    for protocol in [
        Protocol::Openai,
        Protocol::OpenaiCompatible,
        Protocol::Gemini,
        Protocol::LlamaCpp,
    ] {
        let body = sent(protocol, &with);
        assert_eq!(body, sent(protocol, &without), "{protocol:?}");
        assert!(
            !body.to_string().contains("cache_control"),
            "{protocol:?}: {body}"
        );
    }
    // The system blocks still reach them joined by a blank line, as the
    // Messages ingress joined them before each was a part of its own.
    let joined = "You are terse.\n\nHere is the manual.";
    assert_eq!(
        sent(Protocol::OpenaiCompatible, &with)["messages"][0],
        json!({"role": "system", "content": joined})
    );
    assert_eq!(
        sent(Protocol::LlamaCpp, &with)["messages"][0],
        json!({"role": "system", "content": joined})
    );
    assert_eq!(sent(Protocol::Openai, &with)["instructions"], json!(joined));
    assert_eq!(
        sent(Protocol::Gemini, &with)["systemInstruction"],
        json!({"parts": [{"text": joined}]})
    );
}

#[test]
fn an_openai_system_message_of_parts_reaches_every_egress_as_before() {
    let ir = parse_chat_request(&json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": [
                {"type": "text", "text": "one"},
                {"type": "text", "text": "two"},
            ]},
            {"role": "user", "content": "hi"},
        ],
    }))
    .unwrap();
    // Joined by a line break, as the OpenAI wire's system text always was.
    assert_eq!(
        sent(Protocol::OpenaiCompatible, &ir)["messages"][0],
        json!({"role": "system", "content": "one\ntwo"})
    );
    assert_eq!(
        sent(Protocol::LlamaCpp, &ir)["messages"][0]["content"],
        json!("one\ntwo")
    );
    assert_eq!(
        sent(Protocol::Openai, &ir)["instructions"],
        json!("one\ntwo")
    );
    assert_eq!(
        sent(Protocol::Gemini, &ir)["systemInstruction"],
        json!({"parts": [{"text": "one\ntwo"}]})
    );
    assert_eq!(sent(Protocol::Anthropic, &ir)["system"], json!("one\ntwo"));
}

#[test]
fn several_unmarked_system_messages_reach_anthropic_as_the_joined_string() {
    let ir = parse_chat_request(&json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "one"},
            {"role": "developer", "content": "two"},
            {"role": "user", "content": "hi"},
        ],
    }))
    .unwrap();
    assert_eq!(ir.system_text().as_deref(), Some("one\n\ntwo"));
    assert_eq!(
        sent(Protocol::Anthropic, &ir)["system"],
        json!("one\n\ntwo")
    );
}

#[test]
fn a_system_message_of_two_parts_or_a_marked_part_is_sent_as_blocks() {
    let five = json!({"type": "ephemeral"});
    let two_parts = json!({
        "model": "m", "max_tokens": 8,
        "system": [
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two"},
        ],
        "messages": [{"role": "user", "content": "hi"}],
    });
    let body = sent(
        Protocol::Anthropic,
        &parse_messages_request(&two_parts).unwrap(),
    );
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "one"}, {"type": "text", "text": "two"}])
    );

    // Two System messages of which one part is marked.
    let mut ir = parse_chat_request(&json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "one"},
            {"role": "developer", "content": "two"},
            {"role": "user", "content": "hi"},
        ],
    }))
    .unwrap();
    if let crate::ir::ContentPart::Text { cache_control, .. } = &mut ir.messages[1].content[0] {
        *cache_control = Some(crate::ir::CacheControl(five.clone()));
    }
    assert_eq!(
        sent(Protocol::Anthropic, &ir)["system"],
        json!([{"type": "text", "text": "one"}, {"type": "text", "text": "two", "cache_control": five}])
    );
}

/// A mark is no prompt text: lmgw's own estimate of a marked request is
/// the estimate of the same request unmarked, on an Anthropic row and off
/// one.
#[test]
fn a_mark_counts_as_no_estimated_tokens() {
    let with = parse_messages_request(&marked()).unwrap();
    let without = parse_messages_request(&unmarked()).unwrap();
    let row = crate::ir::Signer::new("https://api.anthropic.com");
    for at in [Some(&row), None] {
        assert_eq!(
            crate::proxy::estimate_text(&with, at),
            crate::proxy::estimate_text(&without, at),
        );
    }
}
