//! `ir::compaction::folded`'s rules, and the bodies the egresses without a
//! compaction block build from a compacted conversation.

use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::ingress::anthropic::parse_messages_request;
use lmgw_core::ir::compaction::folded;
use lmgw_core::ir::{Compaction, ContentPart, Role};
use serde_json::{json, Value};

use super::block;

fn ir(messages: Value) -> lmgw_core::ir::ChatRequest {
    parse_messages_request(&json!({
        "model": "m", "max_tokens": 64, "system": "Be brief.", "messages": messages,
    }))
    .unwrap()
}

/// The (role, texts) of each message.
fn shape(r: &lmgw_core::ir::ChatRequest) -> Vec<(Role, Vec<String>)> {
    r.messages
        .iter()
        .map(|m| {
            let texts = m
                .content
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text, .. } => text.clone(),
                    other => format!("{other:?}"),
                })
                .collect();
            (m.role, texts)
        })
        .collect()
}

fn summary() -> String {
    block()["content"].as_str().unwrap().to_string()
}

#[test]
fn a_block_round_trips_through_the_ir_unchanged() {
    let mut b = block();
    b["encrypted_content"] = json!("opaque==");
    b["tool_changes"] = json!([{"type": "tool_addition",
                                "tool": {"type": "tool_reference", "name": "search"}}]);
    let c = Compaction::from_block(&b);
    assert_eq!(c.summary.as_deref(), Some(summary().as_str()));
    assert_eq!(c.to_block(), b);
    // A null summary stays null.
    let none = json!({"type": "compaction", "content": null});
    assert_eq!(Compaction::from_block(&none).to_block(), none);
    // Through the IR's own serialization (a stored thread) too.
    let part = ContentPart::Compaction(c.clone());
    let back: ContentPart = serde_json::from_value(serde_json::to_value(&part).unwrap()).unwrap();
    assert_eq!(back, part);
}

#[test]
fn the_last_block_with_a_summary_drops_everything_before_it() {
    let r = ir(json!([
        {"role": "user", "content": "q1"},
        {"role": "assistant", "content": [
            {"type": "compaction", "content": "an older summary"},
            {"type": "text", "text": "a1"},
        ]},
        {"role": "user", "content": [
            {"type": "text", "text": "q2"},
            block(),
            {"type": "text", "text": "after, same message"},
        ]},
        {"role": "assistant", "content": [
            {"type": "compaction", "content": null},
            {"type": "text", "text": "a2"},
        ]},
    ]));
    let out = folded(&r);
    assert_eq!(
        shape(&out),
        [
            (Role::System, vec!["Be brief.".to_string()]),
            (
                Role::User,
                vec![summary(), "after, same message".to_string()]
            ),
            (Role::Assistant, vec!["a2".to_string()]),
        ]
    );
    // A request without a block is the request itself.
    let plain = ir(json!([{"role": "user", "content": "hi"}]));
    assert!(matches!(folded(&plain), std::borrow::Cow::Borrowed(_)));
}

/// A threshold block leads the assistant answer it came with: the summary
/// is a user turn of its own, the rest of that answer an assistant turn
/// after it.
#[test]
fn the_summary_is_a_user_turn_before_what_followed_the_block() {
    let r = ir(json!([
        {"role": "user", "content": "q1"},
        {"role": "assistant", "content": [block(), {"type": "text", "text": "a1"}]},
        {"role": "user", "content": "q2"},
    ]));
    assert_eq!(
        shape(&folded(&r)),
        [
            (Role::System, vec!["Be brief.".to_string()]),
            (Role::User, vec![summary()]),
            (Role::Assistant, vec!["a1".to_string()]),
            (Role::User, vec!["q2".to_string()]),
        ]
    );
}

/// An on-demand block alone in its assistant message: the summary opens the
/// next user message, so the conversation starts with one user turn.
#[test]
fn a_summary_alone_merges_into_the_next_user_message() {
    let r = ir(json!([
        {"role": "assistant", "content": [block()]},
        {"role": "user", "content": "Go on."},
    ]));
    assert_eq!(
        shape(&folded(&r)),
        [
            (Role::System, vec!["Be brief.".to_string()]),
            (Role::User, vec![summary(), "Go on.".to_string()]),
        ]
    );
}

/// An empty summary is in effect (the history before it is dropped, as
/// Anthropic would) but adds no empty text part.
#[test]
fn an_empty_summary_drops_the_history_and_adds_nothing() {
    let r = ir(json!([
        {"role": "user", "content": "q1"},
        {"role": "assistant", "content": [
            {"type": "compaction", "content": ""},
            {"type": "text", "text": "a1"},
        ]},
        {"role": "user", "content": "q2"},
    ]));
    assert_eq!(
        shape(&folded(&r)),
        [
            (Role::System, vec!["Be brief.".to_string()]),
            (Role::Assistant, vec!["a1".to_string()]),
            (Role::User, vec!["q2".to_string()]),
        ]
    );
    assert_eq!(
        sent(Protocol::OpenaiCompatible, &r)["messages"],
        json!([
            {"role": "system", "content": "Be brief."},
            {"role": "assistant", "content": "a1"},
            {"role": "user", "content": "q2"},
        ])
    );
}

fn upstream(protocol: Protocol) -> Upstream {
    Upstream {
        id: 9,
        name: "u".into(),
        protocol,
        kind: UpstreamKind::Generic,
        base_url: "https://upstream.test".into(),
        api_key: Some("k".into()),
        extra_headers: vec![],
        timeout_ms: 30_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn sent(protocol: Protocol, r: &lmgw_core::ir::ChatRequest) -> Value {
    let req = for_protocol(protocol)
        .build_chat(
            &reqwest::Client::new(),
            &upstream(protocol),
            "tgt",
            r,
            &r.params,
            false,
        )
        .unwrap()
        .build()
        .unwrap();
    serde_json::from_slice(req.body().and_then(reqwest::Body::as_bytes).unwrap()).unwrap()
}

/// The on-demand shape: the block alone first, the next turn after it. The
/// summary opens that user turn, so every egress gets a conversation that
/// starts with the user (gateway design §6.1).
#[test]
fn gemini_llama_cpp_and_the_platform_get_the_summary_in_the_first_user_turn() {
    let r = ir(json!([
        {"role": "assistant", "content": [block()]},
        {"role": "user", "content": "Go on."},
    ]));
    let gemini = sent(Protocol::Gemini, &r);
    assert_eq!(
        gemini["contents"],
        json!([
            {"role": "user", "parts": [{"text": summary()}, {"text": "Go on."}]},
        ])
    );
    let llama = sent(Protocol::LlamaCpp, &r);
    assert_eq!(
        llama["messages"][0],
        json!({"role": "system", "content": "Be brief."})
    );
    assert_eq!(llama["messages"][1]["role"], "user", "{llama}");
    assert_eq!(llama["messages"].as_array().unwrap().len(), 2, "{llama}");
    let first = llama["messages"][1]["content"].to_string();
    assert!(
        first.find(&summary()).unwrap() < first.find("Go on.").unwrap(),
        "{first}"
    );
    let platform = sent(Protocol::Openai, &r);
    let text = platform.to_string();
    assert!(text.contains(&summary()), "{text}");
    assert!(!text.contains("signature"), "{text}");
    assert!(!text.contains("\"assistant\""), "{text}");
}

/// Anthropic gets the block back as it came, first in its message.
#[test]
fn anthropic_gets_the_block_as_it_came() {
    let r = ir(json!([
        {"role": "user", "content": "q1"},
        {"role": "assistant", "content": [block(), {"type": "text", "text": "a1"}]},
        {"role": "user", "content": "q2"},
    ]));
    let body = sent(Protocol::Anthropic, &r);
    assert_eq!(body["messages"][1]["content"][0], block());
    assert_eq!(body["messages"][0]["content"][0]["text"], "q1");
}
