//! A tool result's MCP `content` blocks, for a client that hands the result
//! to an MCP Apps view (client-apps design §7.5): the server's own blocks
//! when the call reached one, else lmgw's IR blocks said in MCP's shape —
//! `blocks_from_result`'s way back, so a built-in toolset's result or a
//! refusal reads like a server's.

use serde_json::{json, Value};

use crate::ir::ToolResultBlock;

/// A server's `CallToolResult.content`, as JSON values: each block as the
/// server sent it (annotations and `_meta` kept), after `/mcp`'s namespacing
/// of its resource URIs.
pub(crate) fn of_result(result: &rmcp::model::CallToolResult) -> Vec<Value> {
    result
        .content
        .iter()
        .filter_map(|c| serde_json::to_value(c).ok())
        .collect()
}

/// `blocks` as MCP content blocks. A JSON block is the one MCP has no type
/// for: one `blocks_from_result` kept from a block type it did not know is
/// that block again, any other its JSON as text (MCP's own advice for
/// structured data in `content`).
pub(crate) fn of_blocks(blocks: &[ToolResultBlock]) -> Vec<Value> {
    blocks
        .iter()
        .map(|b| match b {
            ToolResultBlock::Text { text } => json!({"type": "text", "text": text}),
            ToolResultBlock::Image { mime, data } => {
                json!({"type": "image", "data": data, "mimeType": mime})
            }
            ToolResultBlock::Audio { mime, data } => {
                json!({"type": "audio", "data": data, "mimeType": mime})
            }
            ToolResultBlock::Resource { uri, mime, text } => {
                let mut r = json!({"uri": uri});
                if let Some(m) = mime {
                    r["mimeType"] = json!(m);
                }
                if let Some(t) = text {
                    r["text"] = json!(t);
                }
                json!({"type": "resource", "resource": r})
            }
            ToolResultBlock::Json { value } if value.get("type").is_some_and(Value::is_string) => {
                value.clone()
            }
            ToolResultBlock::Json { value } => json!({
                "type": "text",
                "text": serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ir_blocks_read_as_mcp_content() {
        let blocks = vec![
            ToolResultBlock::text("hi"),
            ToolResultBlock::Image {
                mime: "image/png".into(),
                data: "AAAA".into(),
            },
            ToolResultBlock::Resource {
                uri: "file://p__/a".into(),
                mime: None,
                text: Some("x".into()),
            },
            ToolResultBlock::Json {
                value: json!({"type": "resource_link", "uri": "ui://p__w", "name": "w"}),
            },
            ToolResultBlock::Json {
                value: json!({"temp": 21}),
            },
        ];
        assert_eq!(
            of_blocks(&blocks),
            vec![
                json!({"type": "text", "text": "hi"}),
                json!({"type": "image", "data": "AAAA", "mimeType": "image/png"}),
                json!({"type": "resource", "resource": {"uri": "file://p__/a", "text": "x"}}),
                json!({"type": "resource_link", "uri": "ui://p__w", "name": "w"}),
                json!({"type": "text", "text": "{\"temp\":21}"}),
            ]
        );
    }

    #[test]
    fn a_server_s_blocks_are_kept_as_sent() {
        let r: rmcp::model::CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "text", "text": "sunny", "annotations": {"audience": ["user"]}},
                {"type": "image", "data": "AAAA", "mimeType": "image/png"}
            ],
            "structuredContent": {"temp": 21}
        }))
        .unwrap();
        let c = of_result(&r);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["annotations"]["audience"], json!(["user"]));
        assert_eq!(c[1]["data"], "AAAA");
    }
}
