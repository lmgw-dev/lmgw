//! An Anthropic stream's server tool blocks (`server_tool_use`,
//! `mcp_tool_use`: the provider's own tools) are verbatim blocks
//! (`ir::AnthropicBlock`, `anthropic_verbatim_blocks`): their
//! `input_json_delta`s go with them and never reach a client's tool call,
//! which carries exactly its own arguments.

use lmgw_core::config::Protocol;
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::StreamDelta;
use lmgw_core::sse::SseEvent;
use serde_json::{json, Value};

fn start(index: u64, block: Value) -> Value {
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

fn args(index: u64, partial: &str) -> Value {
    json!({"type": "content_block_delta", "index": index,
           "delta": {"type": "input_json_delta", "partial_json": partial}})
}

fn stop(index: u64) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

#[test]
fn a_server_tool_s_input_never_reaches_the_client_s_tool_call() {
    let mut dec = for_protocol(Protocol::Anthropic).new_decoder();
    let events = [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 9}}}),
        start(
            0,
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}),
        ),
        args(0, "{\"query\": \"oslo weather\"}"),
        stop(0),
        start(
            1,
            json!({"type": "mcp_tool_use", "id": "mcptoolu_1", "name": "lookup",
                   "server_name": "s", "input": {}}),
        ),
        args(1, "{\"q\": 1}"),
        stop(1),
        start(
            2,
            json!({"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}),
        ),
        args(2, "{\"city\":"),
        args(2, " \"Oslo\"}"),
        stop(2),
    ];
    let mut deltas = Vec::new();
    for e in events {
        deltas.extend(dec.on_event(&SseEvent {
            event: Some(e["type"].as_str().unwrap().into()),
            data: e.to_string(),
        }));
    }
    let calls: Vec<&StreamDelta> = deltas
        .iter()
        .filter(|d| {
            matches!(
                d,
                StreamDelta::ToolCallStart { .. } | StreamDelta::ToolCallArgsDelta { .. }
            )
        })
        .collect();
    assert_eq!(
        calls,
        [
            &StreamDelta::ToolCallStart {
                index: 0,
                id: "toolu_1".into(),
                name: "get_weather".into(),
            },
            &StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: "{\"city\":".into(),
            },
            &StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: " \"Oslo\"}".into(),
            },
        ]
    );
}
