//! The placeholders a fallback that cannot see gets, and what they leave alone.

use super::*;
use crate::config::{Protocol, UpstreamKind};
use crate::ir::{AnthropicBlock, ImageSource, Message, Params, Role};

/// A fallback row of `protocol`.
fn row(protocol: Protocol) -> Upstream {
    Upstream {
        id: 1,
        name: "cloud".into(),
        protocol,
        kind: UpstreamKind::Generic,
        base_url: "https://api.example.com/v1".into(),
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 1000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

/// An OpenAI-shaped fallback, which is sent no Anthropic block.
fn openai() -> Upstream {
    row(Protocol::OpenaiCompatible)
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model_alias: "local-vl".into(),
        messages,
        params: Params::default(),
        tools: Vec::new(),
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

fn unseen() -> Unseen {
    Unseen {
        fallback: "cloud-text".into(),
        requested: "local-vl".into(),
    }
}

fn inline(data: &str) -> ContentPart {
    ContentPart::Image {
        mime: "image/png".into(),
        source: ImageSource::Base64 { data: data.into() },
    }
}

fn tool_result(blocks: Vec<ToolResultBlock>) -> Message {
    Message {
        role: Role::Tool,
        content: vec![ContentPart::ToolResult {
            id: "call_1".into(),
            name: Some("screenshot".into()),
            content: blocks,
            is_error: false,
        }],
    }
}

/// The reason goes to a provider: it names no alias of the owner's.
#[test]
fn the_reason_names_no_model() {
    assert_eq!(PLACEHOLDER_REASON, "the answering model cannot see images");
}

#[test]
fn a_user_image_becomes_the_placeholder_a_tool_image_gets() {
    let ir = request(vec![Message {
        role: Role::User,
        content: vec![ContentPart::text("what is this"), inline("iVBORw0KGgo=")],
    }]);
    let (out, dropped) = without_images(&ir, &openai());
    assert_eq!(
        out.messages[0].content,
        vec![
            ContentPart::text("what is this"),
            ContentPart::text(tool_image_placeholder(
                "image/png",
                "iVBORw0KGgo=",
                PLACEHOLDER_REASON
            )),
        ]
    );
    assert_eq!(
        out.messages[0].content[1],
        ContentPart::text(
            "[image/png image, 12 base64 bytes — omitted: the answering model cannot see images]"
        )
    );
    assert_eq!(
        dropped,
        vec!["image/png image, 12 base64 bytes".to_string()]
    );
    assert!(!carries_images(&out, &openai()));
}

#[test]
fn an_image_by_url_says_so_without_the_url() {
    let ir = request(vec![Message {
        role: Role::User,
        content: vec![ContentPart::Image {
            mime: "image/jpeg".into(),
            source: ImageSource::Url {
                url: "https://example.com/cat.jpg?sig=secret".into(),
            },
        }],
    }]);
    let (out, dropped) = without_images(&ir, &openai());
    let ContentPart::Text { text, .. } = &out.messages[0].content[0] else {
        panic!("{:?}", out.messages[0].content);
    };
    assert!(
        text.starts_with("[image/jpeg image by URL — omitted: "),
        "{text}"
    );
    assert!(!text.contains("example.com"), "{text}");
    assert_eq!(dropped, vec!["image/jpeg image by URL".to_string()]);
}

#[test]
fn a_tool_results_image_becomes_its_placeholder_and_its_text_stays() {
    let ir = request(vec![tool_result(vec![
        ToolResultBlock::text("the screen:"),
        ToolResultBlock::Image {
            mime: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        },
    ])]);
    let (out, dropped) = without_images(&ir, &openai());
    let ContentPart::ToolResult { content, .. } = &out.messages[0].content[0] else {
        panic!("{:?}", out.messages[0].content);
    };
    assert_eq!(
        content,
        &vec![
            ToolResultBlock::text("the screen:"),
            ToolResultBlock::text(tool_image_placeholder(
                "image/png",
                "iVBORw0KGgo=",
                PLACEHOLDER_REASON
            )),
        ]
    );
    assert_eq!(dropped.len(), 1);
    assert!(carries_images(&ir, &openai()));
    assert!(!carries_images(&out, &openai()));
}

fn file(ty: &str) -> AnthropicBlock {
    AnthropicBlock::from_client(serde_json::json!({
        "type": ty, "source": {"type": "file", "file_id": "file_secret1"},
    }))
}

/// A Files API image (a verbatim Anthropic block) is an image on an
/// Anthropic fallback, which is sent it: it counts, and becomes a
/// placeholder without its file id, a tool result's as a message's, and a
/// document beside it is left alone.
#[test]
fn a_files_api_image_is_an_image_where_it_is_sent() {
    let part = |ty: &str| ContentPart::AnthropicBlock(file(ty));
    let ir = request(vec![
        Message {
            role: Role::User,
            content: vec![
                part("image"),
                part("document"),
                ContentPart::text("compare"),
            ],
        },
        tool_result(vec![ToolResultBlock::AnthropicBlock(file("image"))]),
    ]);
    let anthropic = row(Protocol::Anthropic);
    assert_eq!(count_images(&ir, &anthropic), 2);
    assert_eq!(crate::gate::media_parts(&ir, Some(&anthropic)).images, 1);
    let (out, dropped) = without_images(&ir, &anthropic);
    assert_eq!(
        dropped,
        [
            "image by Files API reference",
            "image by Files API reference"
        ]
    );
    let placeholder =
        "[image by Files API reference — omitted: the answering model cannot see images]";
    assert_eq!(
        out.messages[0].content,
        [
            ContentPart::text(placeholder),
            part("document"),
            ContentPart::text("compare"),
        ]
    );
    assert_eq!(
        out.messages[1],
        tool_result(vec![ToolResultBlock::text(placeholder)])
    );
    assert!(!carries_images(&out, &anthropic));
}

/// Every other row is sent none of a Files API image (the egress names it
/// `content.image`), so to it the block is no image: nothing counts it, no
/// vision is asked of it and it gets no placeholder (changed 2026-11-06).
#[test]
fn a_files_api_image_is_no_image_where_it_is_not_sent() {
    let ir = request(vec![
        Message {
            role: Role::User,
            content: vec![
                ContentPart::AnthropicBlock(file("image")),
                ContentPart::text("what"),
            ],
        },
        tool_result(vec![ToolResultBlock::AnthropicBlock(file("image"))]),
    ]);
    for up in [openai(), row(Protocol::Gemini), row(Protocol::LlamaCpp)] {
        assert_eq!(count_images(&ir, &up), 0, "{:?}", up.protocol);
        assert!(!carries_images(&ir, &up));
        assert_eq!(crate::gate::media_parts(&ir, Some(&up)).images, 0);
        let (out, dropped) = without_images(&ir, &up);
        assert_eq!(out, ir);
        assert!(dropped.is_empty());
    }
    assert_eq!(crate::gate::media_parts(&ir, None).images, 0);
    let facets = crate::gate::request_facets(&ir, None);
    assert!(!facets.contains(crate::candidates::Facet::Vision));
}

#[test]
fn audio_and_text_are_not_images() {
    let ir = request(vec![Message {
        role: Role::User,
        content: vec![
            ContentPart::text("listen"),
            ContentPart::Audio {
                mime: "audio/wav".into(),
                data: "UklGRg==".into(),
            },
        ],
    }]);
    assert!(!carries_images(&ir, &openai()));
    let (out, dropped) = without_images(&ir, &openai());
    assert_eq!(out, ir);
    assert!(dropped.is_empty());
}

/// A tool loop sends its conversation again on every call: each send gets
/// every placeholder, the count is the send's, and only the images not
/// named before are new to the turn.
#[test]
fn a_turn_names_each_image_once() {
    let first = request(vec![Message {
        role: Role::User,
        content: vec![inline("AAAA"), inline("BBBB")],
    }]);
    let mut second = first.clone();
    second
        .messages
        .push(tool_result(vec![ToolResultBlock::Image {
            mime: "image/png".into(),
            data: "CCCC".into(),
        }]));
    let turn = Announced::default();
    let (out, n) = turn.without_images(&first, &openai(), &unseen());
    assert_eq!((n, turn.count()), (2, 2));
    assert!(!carries_images(&out, &openai()));
    let (out, n) = turn.without_images(&second, &openai(), &unseen());
    assert_eq!((n, turn.count()), (3, 3));
    assert!(!carries_images(&out, &openai()));
    assert_eq!(count_images(&second, &openai()), 3);
    // The same conversation again names nothing new.
    let (_, n) = turn.without_images(&second, &openai(), &unseen());
    assert_eq!((n, turn.count()), (3, 3));
}
