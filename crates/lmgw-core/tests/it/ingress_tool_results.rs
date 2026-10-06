//! The two tool-result ingress fixes (llama-egress design §8.3, WP5).
//!
//! - The Responses ingress reads an array `output` of a `function_call_output`
//!   into blocks, as llama-server's own converter does, instead of turning it
//!   into its JSON text with an image's base64 inside.
//! - The OpenAI ingress keeps only the text of a tool message, as OpenAI
//!   defines it, and names every part it drops in a WARN.
//! - The Anthropic ingress names a `tool_result` image given by URL as a
//!   resource, as the Responses ingress does, instead of an image with no
//!   bytes.

use lmgw_core::ingress::{anthropic, openai, responses};
use lmgw_core::ir::{ContentPart, ToolResultBlock};
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::common::captured_log::capture_log;
use crate::responses_api::{mount_sequence, post, setup, text_reply};

/// A one-pixel PNG's first bytes, base64: what a tool's screenshot looks like
/// on the wire, short enough to read in an assertion.
const PNG_B64: &str = "iVBORw0KGgo=";
/// A whole 1x1 GIF.
const GIF_B64: &str = "R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==";
/// A JFIF JPEG's first bytes.
const JPEG_B64: &str = "/9j/4AAQSkZJRgABAQA=";
/// A BMP's first bytes.
const BMP_B64: &str = "Qk06AAAAAAAAADYAAAA=";

/// The tool-result blocks of the request's only `function_call_output`.
fn output_blocks(output: Value) -> Vec<ToolResultBlock> {
    let req = responses::parse_request(&json!({
        "model": "m",
        "input": [
            {"role": "user", "content": "Show me the chart."},
            {"type": "function_call", "call_id": "call_1", "name": "chart", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": output},
        ],
    }))
    .unwrap();
    let tool = req.ir.messages.last().unwrap();
    match tool.content.as_slice() {
        [ContentPart::ToolResult { id, content, .. }] => {
            assert_eq!(id, "call_1");
            content.clone()
        }
        other => panic!("not one tool result: {other:?}"),
    }
}

#[test]
fn responses_reads_text_and_a_data_url_image_into_blocks_and_keeps_an_unknown_item() {
    let unknown = json!({"type": "input_file", "filename": "notes.txt", "file_data": "aGk="});
    let (log, capturing) = capture_log();
    let blocks = output_blocks(json!([
        {"type": "input_text", "text": "The chart:"},
        {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_B64}"),
         "detail": "auto"},
        unknown.clone(),
    ]));
    drop(capturing);
    assert_eq!(
        blocks,
        vec![
            ToolResultBlock::text("The chart:"),
            ToolResultBlock::Image {
                mime: "image/png".into(),
                data: PNG_B64.into(),
            },
            // Today's JSON text, of the item alone.
            ToolResultBlock::text(unknown.to_string()),
        ]
    );
    let log = log.text();
    assert!(
        log.contains("WARN") && log.contains("#2 input_file") && log.contains("call_1"),
        "{log}"
    );
}

#[test]
fn responses_names_a_url_image_as_a_resource_and_keeps_what_it_cannot_read_as_json_text() {
    let by_file_id = json!({"type": "input_image", "file_id": "file-abc"});
    let svg_utf8 = json!({"type": "input_image",
                          "image_url": "data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg'/>"});
    let no_text = json!({"type": "input_text"});
    let (log, capturing) = capture_log();
    let blocks = output_blocks(json!([
        {"type": "input_image", "image_url": "https://example.com/chart.webp?size=2#top"},
        {"type": "input_image", "image_url": {"url": "https://example.com/render?id=3"}},
        {"type": "input_image", "image_url": format!("data:;base64,{GIF_B64}")},
        by_file_id.clone(),
        svg_utf8.clone(),
        no_text.clone(),
    ]));
    drop(capturing);
    assert_eq!(
        blocks,
        vec![
            ToolResultBlock::Resource {
                uri: "https://example.com/chart.webp?size=2#top".into(),
                mime: Some("image/webp".into()),
                text: None,
            },
            // The path names no type: an image, without a guess at which.
            ToolResultBlock::Resource {
                uri: "https://example.com/render?id=3".into(),
                mime: Some("image/*".into()),
                text: None,
            },
            // RFC 2397's default type left out: the bytes say which.
            ToolResultBlock::Image {
                mime: "image/gif".into(),
                data: GIF_B64.into(),
            },
            ToolResultBlock::text(by_file_id.to_string()),
            ToolResultBlock::text(svg_utf8.to_string()),
            ToolResultBlock::text(no_text.to_string()),
        ]
    );
    let log = log.text();
    assert!(
        log.contains("#3 input_image, #4 input_image, #5 input_text"),
        "{log}"
    );
}

/// Only an image Anthropic's `media_type` takes becomes an image block:
/// png, jpeg, gif or webp whose bytes are that format, its mime the
/// format's own lowercase one. An SVG, a BMP, a mislabelled image and base64
/// that does not decode stay their JSON text, with the WARN, as before.
#[test]
fn responses_takes_an_image_only_in_a_format_anthropic_takes_and_its_bytes_are() {
    let item = |url: String| json!({"type": "input_image", "image_url": url});
    let kept = [
        item("data:image/svg+xml;base64,PHN2Zy8+".into()),
        item(format!("data:image/bmp;base64,{BMP_B64}")),
        item(format!("data:image/png;base64,{JPEG_B64}")),
        item("data:image/png;base64,iVBORw0KGgo".into()),
        item("data:image/png;base64,not base64!".into()),
    ];
    let (log, capturing) = capture_log();
    let mut output = vec![
        item(format!("data:image/jpg;base64,{JPEG_B64}")),
        item(format!("data:IMAGE/PNG;base64,{PNG_B64}")),
    ];
    output.extend(kept.iter().cloned());
    let blocks = output_blocks(Value::Array(output));
    drop(capturing);
    let mut want = vec![
        ToolResultBlock::Image {
            mime: "image/jpeg".into(),
            data: JPEG_B64.into(),
        },
        ToolResultBlock::Image {
            mime: "image/png".into(),
            data: PNG_B64.into(),
        },
    ];
    want.extend(kept.iter().map(|k| ToolResultBlock::text(k.to_string())));
    assert_eq!(blocks, want);
    let log = log.text();
    assert!(
        log.contains(
            "#2 input_image, #3 input_image, #4 input_image, #5 input_image, #6 input_image"
        ),
        "{log}"
    );
}

#[test]
fn responses_keeps_every_non_array_output_as_it_was() {
    assert_eq!(output_blocks(json!("hi")), ToolResultBlock::one("hi"));
    assert_eq!(
        output_blocks(json!({"ok": true})),
        ToolResultBlock::one(r#"{"ok":true}"#)
    );
    assert_eq!(output_blocks(Value::Null), ToolResultBlock::one("null"));
    // An empty array is an empty result, as MCP's empty result looks.
    assert_eq!(output_blocks(json!([])), ToolResultBlock::one(""));
}

/// The upstream sees the image's placeholder, never its base64: the OpenAI
/// egress flattens a tool result to text and names what it left out.
#[tokio::test]
async fn a_responses_tool_image_reaches_a_text_only_upstream_as_its_placeholder() {
    let mock = MockServer::start().await;
    mount_sequence(&mock, vec![text_reply("a rising line")]).await;
    let (_state, base) = setup(&mock.uri()).await;

    let (status, resp) = post(
        &base,
        json!({
            "model": "my-model",
            "input": [
                {"role": "user", "content": "Show me the chart."},
                {"type": "function_call", "call_id": "call_1", "name": "chart",
                 "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": [
                    {"type": "input_text", "text": "The chart:"},
                    {"type": "input_image",
                     "image_url": format!("data:image/png;base64,{PNG_B64}")},
                ]},
            ],
            "tools": [{"type": "function", "name": "chart",
                       "parameters": {"type": "object"}}],
        }),
    )
    .await;
    assert_eq!(status, 200, "{resp}");

    let raw = mock.received_requests().await.unwrap()[0].body.clone();
    let sent: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(sent["messages"][2]["role"], "tool");
    assert_eq!(
        sent["messages"][2]["content"],
        "The chart:\n[image/png image, 12 base64 bytes — omitted: this upstream's \
         tool-result slot is text-only]"
    );
    assert!(!String::from_utf8_lossy(&raw).contains(PNG_B64));
}

#[test]
fn openai_keeps_the_text_of_a_tool_message_and_warns_about_every_part_it_drops() {
    let (log, capturing) = capture_log();
    let ir = openai::parse_chat_request(&json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "Show me the chart."},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_7", "type": "function",
                 "function": {"name": "chart", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "call_7", "content": [
                {"type": "text", "text": "The chart:"},
                {"type": "image_url",
                 "image_url": {"url": format!("data:image/png;base64,{PNG_B64}")}},
                {"type": "text", "text": "Rising."},
                {"type": "input_audio", "input_audio": {"data": "QUFB", "format": "wav"}},
            ]},
        ],
    }))
    .unwrap();
    drop(capturing);
    assert!(matches!(
        &ir.messages[2].content[0],
        ContentPart::ToolResult { id, content, .. }
            if id == "call_7" && content == &ToolResultBlock::one("The chart:\nRising.")
    ));
    let log = log.text();
    assert!(
        log.contains("WARN")
            && log.contains("call_7")
            && log.contains("#1 image_url, #3 input_audio"),
        "{log}"
    );
    assert!(!log.contains(PNG_B64), "{log}");
}

#[test]
fn openai_tool_message_of_text_alone_warns_about_nothing() {
    let (log, capturing) = capture_log();
    let ir = openai::parse_chat_request(&json!({
        "model": "m",
        "messages": [
            {"role": "tool", "tool_call_id": "call_8", "content": "12°C"},
            {"role": "tool", "tool_call_id": "call_9", "content": [
                {"type": "text", "text": "13°C"}]},
        ],
    }))
    .unwrap();
    drop(capturing);
    assert!(matches!(
        &ir.messages[1].content[0],
        ContentPart::ToolResult { content, .. } if content == &ToolResultBlock::one("13°C")
    ));
    assert!(!log.text().contains("dropped"), "{}", log.text());
}

/// The tool-result blocks of an Anthropic request whose one `tool_result`
/// carries `content`.
fn anthropic_tool_result(content: Value) -> Vec<ToolResultBlock> {
    let req = anthropic::parse_messages_request(&json!({
        "model": "m",
        "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "Show me the chart."},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "chart", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": content}]},
        ],
    }))
    .unwrap();
    req.messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|p| match p {
            ContentPart::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("a tool result")
}

/// A `tool_result` image by URL is the resource naming it, as a Responses
/// URL image is; a base64 one stays the image; a source this ingress does
/// not read (a Files API `file`, a URL with no URL) stays the block's JSON.
#[test]
fn anthropic_names_a_url_tool_image_as_a_resource() {
    let file = json!({"type": "image", "source": {"type": "file", "file_id": "file_1"}});
    let no_url = json!({"type": "image", "source": {"type": "url"}});
    let blocks = anthropic_tool_result(json!([
        {"type": "image", "source": {"type": "url", "url": "https://example.com/chart.png?v=2"}},
        {"type": "image", "source": {"type": "base64", "media_type": "image/gif", "data": GIF_B64}},
        file.clone(),
        no_url.clone(),
    ]));
    assert_eq!(
        blocks,
        vec![
            ToolResultBlock::Resource {
                uri: "https://example.com/chart.png?v=2".into(),
                mime: Some("image/png".into()),
                text: None,
            },
            ToolResultBlock::Image {
                mime: "image/gif".into(),
                data: GIF_B64.into(),
            },
            ToolResultBlock::Json { value: file },
            ToolResultBlock::Json { value: no_url },
        ]
    );
}
