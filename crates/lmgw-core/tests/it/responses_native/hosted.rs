//! Hosted tools and the other parts only a native upstream serves (openai
//! egress design §6.1, WP5b): forwarded as sent on both protocols that take
//! `/v1/responses` natively, refused by lmgw's loop with the error the parser
//! used to give them, refused naming both when the request also needs the
//! loop, and `background: true` refused everywhere.

use crate::support::mcp_stub::{echo_stub, register};

use super::routing::{store_response, STORED};
use super::*;

/// One request per part only a native upstream serves, each with what the
/// loop's refusal names (the parser's own words before WP5b).
fn native_only_requests() -> Vec<(Value, &'static str)> {
    let with_tool = |tool: Value| json!({"model": ALIAS, "input": "go", "tools": [tool]});
    vec![
        (
            with_tool(json!({"type": "web_search"})),
            "tool type Some(\"web_search\")",
        ),
        (
            with_tool(json!({"type": "file_search", "vector_store_ids": ["vs_example"]})),
            "tool type Some(\"file_search\")",
        ),
        (
            with_tool(json!({"type": "code_interpreter", "container": {"type": "auto"}})),
            "tool type Some(\"code_interpreter\")",
        ),
        (
            with_tool(json!({"type": "image_generation", "partial_images": 2})),
            "tool type Some(\"image_generation\")",
        ),
        (
            with_tool(
                json!({"type": "computer_use_preview", "display_width": 1024,
                             "display_height": 768, "environment": "browser"}),
            ),
            "tool type Some(\"computer_use_preview\")",
        ),
        (
            json!({"model": ALIAS, "input": "go", "tool_choice": {"type": "web_search_preview"},
                   "tools": [{"type": "web_search_preview"}]}),
            "tool type Some(\"web_search_preview\")",
        ),
        (
            json!({"model": ALIAS, "input": "go", "truncation": "auto"}),
            "'truncation': \"auto\"",
        ),
        (
            json!({"model": ALIAS, "input": [
                {"role": "user", "content": "find it"},
                {"type": "web_search_call", "id": "ws_1", "status": "completed",
                 "action": {"type": "search", "query": "lmgw"}},
                {"role": "user", "content": "and?"},
            ]}),
            "input item type 'web_search_call'",
        ),
        (
            json!({"model": ALIAS, "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "summarise"},
                {"type": "input_file", "filename": "notes.txt",
                 "file_url": "https://docs.example/notes.txt"},
            ]}]}),
            "input content part Some(\"input_file\")",
        ),
    ]
}

#[tokio::test]
async fn each_native_only_part_goes_to_a_native_upstream_as_sent() {
    for protocol in NATIVE {
        let (base, up) = upstream(Says::Answers).await;
        let (_state, gw) = gateway(protocol, &base).await;
        let requests = native_only_requests();
        for (i, (body, what)) in requests.iter().enumerate() {
            let (status, answer) = post(&gw, body.clone()).await;
            assert_eq!(status, 200, "{protocol:?} {what}: {answer}");
            let sent = &up.responses()[i];
            for key in ["tools", "tool_choice", "truncation", "input"] {
                assert_eq!(sent.get(key), body.get(key), "{protocol:?} {what}: {key}");
            }
            assert_eq!(sent["model"], "gpt-test");
        }
        assert_eq!(up.responses().len(), requests.len());
        assert_eq!(up.chats(), 0, "{protocol:?}: nothing in the loop");
    }
}

/// A compatible row without the flag takes the loop: each part is refused
/// with the parser's old error, and nothing goes upstream.
#[tokio::test]
async fn the_loop_refuses_each_with_the_error_it_always_gave() {
    let (base, up) = upstream(Says::Answers).await;
    let (_state, gw) = gateway_flagged(Protocol::OpenaiCompatible, &base, false).await;
    for (body, what) in native_only_requests() {
        let (status, answer) = post(&gw, body).await;
        assert_eq!(status, 400, "{what}: {answer}");
        let msg = answer["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains(what), "{what}: {msg}");
        assert_eq!(answer["error"]["code"], "unsupported", "{answer}");
    }
    let (_, answer) = post(
        &gw,
        json!({"model": ALIAS, "input": "go", "tools": [{"type": "web_search"}]}),
    )
    .await;
    let msg = answer["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("function") && msg.contains("mcp") && msg.contains("hosted"),
        "{msg}"
    );
    assert!(up.responses().is_empty());
    assert_eq!(up.chats(), 0);
}

#[tokio::test]
async fn a_hosted_tool_beside_an_mcp_label_lmgw_serves_is_a_400_naming_both() {
    for protocol in NATIVE {
        let (base, up) = upstream(Says::Answers).await;
        let (state, gw) = gateway(protocol, &base).await;
        let mcp = echo_stub().await;
        register(&state, "stub", "tools", &mcp.url, true, None).await;
        let (status, answer) = post(
            &gw,
            json!({"model": ALIAS, "input": "hi", "tools": [
                {"type": "mcp", "server_label": "tools"},
                {"type": "web_search"},
            ]}),
        )
        .await;
        assert_eq!(status, 400, "{protocol:?}: {answer}");
        let msg = answer["error"]["message"].as_str().unwrap();
        assert!(msg.contains("'tools'"), "the loop's need: {msg}");
        assert!(
            msg.contains("tool type 'web_search'"),
            "the native part: {msg}"
        );
        assert!(msg.contains("native-up"), "{msg}");
        assert!(up.responses().is_empty());
        assert_eq!(up.chats(), 0);
    }
}

#[tokio::test]
async fn a_hosted_tool_continuing_a_chain_lmgw_stored_is_a_400_naming_both() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    store_response(&state).await;
    let (status, answer) = post(
        &gw,
        json!({"model": ALIAS, "input": "and?", "previous_response_id": STORED,
               "tools": [{"type": "code_interpreter", "container": {"type": "auto"}}]}),
    )
    .await;
    assert_eq!(status, 400, "{answer}");
    let msg = answer["error"]["message"].as_str().unwrap();
    assert!(msg.contains(STORED), "{msg}");
    assert!(msg.contains("tool type 'code_interpreter'"), "{msg}");
    assert!(up.responses().is_empty());
    assert_eq!(up.chats(), 0);
}

/// Refused on the native path too, and the error says why.
#[tokio::test]
async fn background_is_refused_on_every_path() {
    for flag in [true, false] {
        let (base, up) = upstream(Says::Answers).await;
        let (_state, gw) = gateway_flagged(Protocol::OpenaiCompatible, &base, flag).await;
        let (status, answer) = post(
            &gw,
            json!({"model": ALIAS, "input": "go", "background": true}),
        )
        .await;
        assert_eq!(status, 400, "{answer}");
        let msg = answer["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("background") && msg.contains("budget"),
            "{msg}"
        );
        assert!(up.responses().is_empty());
    }
    let (base, up) = upstream(Says::Answers).await;
    let (_state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, _) = post(
        &gw,
        json!({"model": ALIAS, "input": "go", "background": true}),
    )
    .await;
    assert_eq!(status, 400);
    assert!(up.responses().is_empty());
}
