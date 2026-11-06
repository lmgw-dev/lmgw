//! Where a compaction block goes (gateway design §6.1, "Where a signature
//! goes", changed 2026-11-06). One lmgw read from an answer, whole or
//! streamed, names the scope that answered ([`Signer`]), as a signed
//! thinking block does: its `encrypted_content` and `signature` are that
//! provider's. lmgw's own records send it to every Anthropic row at that
//! scope as it came; a row at another scope gets the conversation folded as
//! every other egress gets it (the history before the block dropped, its
//! summary a user turn), the request names `content.compaction` and the
//! send's INFO line counts it. A client's own block names no scope and goes
//! as the client wrote it.

use lmgw_core::agent::TurnAccumulator;
use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::*;
use lmgw_core::sse::SseEvent;
use serde_json::{json, Value};

const ENC: &str = "ENC-compaction-0a1b";

fn at(base_url: &str, key: &str) -> Upstream {
    Upstream {
        id: 1,
        name: "up".into(),
        protocol: Protocol::Anthropic,
        kind: UpstreamKind::Generic,
        base_url: base_url.into(),
        api_key: Some(key.into()),
        extra_headers: vec![],
        timeout_ms: 1000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn block() -> Value {
    json!({"type": "compaction", "content": "S.", "encrypted_content": ENC})
}

fn asked_again(compaction: Compaction) -> ChatRequest {
    ChatRequest {
        model_alias: "m".into(),
        messages: vec![
            Message::text(Role::User, "q1"),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::Compaction(compaction), ContentPart::text("a1")],
            },
            Message::text(Role::User, "q2"),
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

fn messages(up: &Upstream, ir: &ChatRequest) -> Value {
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
    let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
    body["messages"].clone()
}

/// The compaction of a whole answer and of a stream, as lmgw records them.
fn recorded(up: &Upstream) -> (Compaction, Compaction) {
    let answer = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
        "content": [block(), {"type": "text", "text": "a1"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1},
    });
    let whole = for_protocol(Protocol::Anthropic)
        .parse_completion_from(up, answer.to_string().as_bytes())
        .unwrap();
    let ContentPart::Compaction(whole) = whole.content[0].clone() else {
        panic!("{:?}", whole.content);
    };
    let mut dec = for_protocol(Protocol::Anthropic).decoder_for(up);
    let mut acc = TurnAccumulator::default();
    for e in [
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
               "role": "assistant", "model": "tgt", "content": [],
               "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": block()}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
               "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ] {
        for d in dec.on_event(&SseEvent {
            event: Some(e["type"].as_str().unwrap().into()),
            data: e.to_string(),
        }) {
            acc.on_delta(&d);
        }
    }
    let streamed = acc.finish("tgt".into());
    let streamed = streamed
        .content
        .iter()
        .find_map(|p| match p {
            ContentPart::Compaction(c) => Some(c.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{:?}", streamed.content));
    (whole, streamed)
}

#[test]
fn an_answer_s_compaction_names_the_scope_that_answered() {
    let up = at("https://api.anthropic.com/v1", "k1");
    let (whole, streamed) = recorded(&up);
    for c in [whole, streamed] {
        assert_eq!(c.scope, Some(Signer::of(&up)));
        assert_eq!(c.to_block(), block(), "the wire block carries no scope");
    }
}

#[tokio::test]
async fn a_recorded_compaction_goes_to_its_scope_and_is_folded_elsewhere() {
    let (log, _guard) = crate::common::captured_log::capture_log();
    let answered = at(
        "https://gateway.ai.cloudflare.com/v1/acct/gw/anthropic",
        "k1",
    );
    let ir = asked_again(recorded(&answered).0);

    // Another key at the same scope: the block as it came.
    let same = at(
        "https://gateway.ai.cloudflare.com/v1/acct/gw/anthropic/",
        "k2",
    );
    assert_eq!(
        messages(&same, &ir),
        json!([
            {"role": "user", "content": [{"type": "text", "text": "q1"}]},
            {"role": "assistant", "content": [block(), {"type": "text", "text": "a1"}]},
            {"role": "user", "content": [{"type": "text", "text": "q2"}]},
        ])
    );
    assert!(ir.blocks_ignored(&same).is_empty());

    // Another scope: folded, as every other egress is sent it.
    let other = at(
        "https://gateway.ai.cloudflare.com/v1/acct/gw2/anthropic",
        "k1",
    );
    let sent = messages(&other, &ir);
    assert_eq!(
        sent,
        json!([
            {"role": "user", "content": [{"type": "text", "text": "S."}]},
            {"role": "assistant", "content": [{"type": "text", "text": "a1"}]},
            {"role": "user", "content": [{"type": "text", "text": "q2"}]},
        ])
    );
    assert!(!sent.to_string().contains(ENC), "{sent}");
    assert_eq!(ir.blocks_ignored(&other), ["content.compaction"]);
    let text = log.text();
    assert!(
        text.contains("INFO") && text.contains("compactions=1"),
        "{text}"
    );
    assert!(text.contains("signed_at=https://gateway.ai.cloudflare.com/v1/acct/gw/anthropic"));

    // A client's block names no scope: it goes as written.
    let client = asked_again(Compaction::from_block(&block()));
    assert_eq!(messages(&other, &client)[1]["content"][0], block());
    assert!(client.blocks_ignored(&other).is_empty());
}
