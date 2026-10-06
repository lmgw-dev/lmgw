//! The placeholders a fallback that cannot see gets, and what they leave alone.

use super::*;
use crate::ir::{ImageSource, Message, Params, Role};

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model_alias: "local-vl".into(),
        messages,
        params: Params::default(),
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
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
    let (out, dropped) = without_images(&ir);
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
    assert!(!carries_images(&out));
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
    let (out, dropped) = without_images(&ir);
    let ContentPart::Text { text } = &out.messages[0].content[0] else {
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
    let (out, dropped) = without_images(&ir);
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
    assert!(carries_images(&ir));
    assert!(!carries_images(&out));
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
    assert!(!carries_images(&ir));
    let (out, dropped) = without_images(&ir);
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
    let (out, n) = turn.without_images(&first, &unseen());
    assert_eq!((n, turn.count()), (2, 2));
    assert!(!carries_images(&out));
    let (out, n) = turn.without_images(&second, &unseen());
    assert_eq!((n, turn.count()), (3, 3));
    assert!(!carries_images(&out));
    assert_eq!(count_images(&second), 3);
    // The same conversation again names nothing new.
    let (_, n) = turn.without_images(&second, &unseen());
    assert_eq!((n, turn.count()), (3, 3));
}
