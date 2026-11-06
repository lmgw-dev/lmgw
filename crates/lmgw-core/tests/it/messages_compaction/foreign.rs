//! Every upstream but Anthropic's: a compaction block in the conversation is
//! sent as Anthropic makes of it (the history before it dropped, the summary
//! a user's text opening what is left), and counted so; a client wire without the block
//! reads an Anthropic answer's summary as text, and its usage counts the
//! compaction Anthropic billed.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{block, post_with, ON_DEMAND};
use crate::messages_context_management::{bodies_at, world};

/// A threshold-compacted history as a Messages client sends it back: the
/// turns before the block, the block leading the answer it came with, the
/// turns after it.
fn compacted_body() -> Value {
    json!({
        "model": "m", "max_tokens": 64,
        "system": "You design data models.",
        "messages": [
            {"role": "user", "content": "OLD QUESTION"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "OLD ANSWER"},
            ]},
            {"role": "user", "content": "SECOND QUESTION"},
            {"role": "assistant", "content": [
                block(),
                {"type": "text", "text": "Kept reply."},
            ]},
            {"role": "user", "content": "New question."},
        ],
    })
}

fn chat_answer() -> Value {
    json!({
        "id": "c", "object": "chat.completion", "model": "tgt",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    })
}

#[tokio::test]
async fn an_openai_compatible_upstream_gets_the_summary_as_a_user_turn_and_nothing_before_it() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_answer()))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;

    let resp = post_with(&gw, "/v1/messages", ON_DEMAND, &compacted_body()).await;
    assert_eq!(resp.status(), 200);
    // Translated, not left out: nothing to name.
    assert!(resp.headers().get("x-lmgw-params-ignored").is_none());

    let sent = bodies_at(&mock, "/chat/completions").await;
    let summary = block()["content"].as_str().unwrap().to_string();
    assert_eq!(
        sent[0]["messages"],
        json!([
            {"role": "system", "content": "You design data models."},
            {"role": "user", "content": summary},
            {"role": "assistant", "content": "Kept reply."},
            {"role": "user", "content": "New question."},
        ]),
        "{}",
        sent[0]
    );
    let text = sent[0].to_string();
    assert!(
        !text.contains("signature") && !text.contains("compaction"),
        "{text}"
    );
}

/// A block without a summary (`content: null`) is a no-op to Anthropic: it is
/// left out, and nothing before it is dropped.
#[tokio::test]
async fn a_block_without_a_summary_drops_nothing() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_answer()))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;

    let body = json!({"model": "m", "max_tokens": 64, "messages": [
        {"role": "user", "content": "First."},
        {"role": "assistant", "content": [
            {"type": "compaction", "content": null},
            {"type": "text", "text": "Reply."},
        ]},
        {"role": "user", "content": "Second."},
    ]});
    let resp = post_with(&gw, "/v1/messages", ON_DEMAND, &body).await;
    assert_eq!(resp.status(), 200);
    let sent = bodies_at(&mock, "/chat/completions").await;
    assert_eq!(
        sent[0]["messages"],
        json!([
            {"role": "user", "content": "First."},
            {"role": "assistant", "content": "Reply."},
            {"role": "user", "content": "Second."},
        ])
    );
}

/// The local count of a route without the block counts what it is sent:
/// the same number as the conversation with the summary written as text.
#[tokio::test]
async fn a_local_count_counts_the_summary_and_nothing_before_it() {
    let mock = MockServer::start().await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let count = |body: Value| {
        let gw = &gw;
        async move {
            let mut body = body;
            body.as_object_mut().unwrap().remove("max_tokens");
            let resp = post_with(gw, "/v1/messages/count_tokens", ON_DEMAND, &body).await;
            assert_eq!(resp.status(), 200);
            resp.json::<Value>().await.unwrap()["input_tokens"]
                .as_u64()
                .unwrap()
        }
    };
    let summary = block()["content"].as_str().unwrap().to_string();
    let as_text = json!({
        "model": "m", "system": "You design data models.",
        "messages": [
            {"role": "user", "content": summary},
            {"role": "assistant", "content": "Kept reply."},
            {"role": "user", "content": "New question."},
        ],
    });
    let with_block = count(compacted_body()).await;
    assert_eq!(with_block, count(as_text).await);
    let mut whole = compacted_body();
    whole["messages"][3]["content"][0] = json!({"type": "text", "text": summary});
    assert!(
        count(whole).await > with_block,
        "the history before the block is not counted"
    );
}

/// An Anthropic answer that ran a compaction, read on Chat Completions: the
/// summary is its text, and its usage counts the compaction's tokens, which
/// Anthropic bills and leaves out of its top-level counts.
#[tokio::test]
async fn chat_completions_reads_the_summary_as_text_and_counts_what_it_cost() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [block(), {"type": "text", "text": "Go on."}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 20, "output_tokens": 3, "iterations": [
                {"type": "compaction", "input_tokens": 144, "output_tokens": 276},
                {"type": "message", "input_tokens": 20, "output_tokens": 3}]},
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let answer: Value = resp.json().await.unwrap();
    let summary = block()["content"].as_str().unwrap().to_string();
    assert_eq!(
        answer["choices"][0]["message"]["content"],
        format!("{summary}Go on.")
    );
    assert_eq!(answer["usage"]["prompt_tokens"], 164, "{answer}");
    assert_eq!(answer["usage"]["completion_tokens"], 279, "{answer}");
}
