use serde_json::json;

use super::*;
use crate::config::Protocol;
use crate::ir::{Message, ReasoningControl, Role, ToolDef};

fn mark() -> Option<CacheControl> {
    Some(CacheControl(json!({"type": "ephemeral"})))
}

fn request(messages: Vec<Message>) -> ChatRequest {
    let mut ir = crate::ingress::anthropic::parse_messages_request(&json!({
        "model": "m", "messages": [{"role": "user", "content": "hi"}],
    }))
    .unwrap();
    ir.messages = messages;
    ir
}

fn tool(fields: Value) -> ToolDef {
    ToolDef {
        name: "t".into(),
        description: None,
        parameters: json!({"type": "object"}),
        strict: None,
        anthropic_fields: fields.as_object().cloned().unwrap_or_default(),
    }
}

#[test]
fn a_null_mark_is_none_and_any_other_is_kept_whole() {
    assert_eq!(CacheControl::of(&json!({"cache_control": null})), None);
    assert_eq!(CacheControl::of(&json!({"text": "a"})), None);
    let ttl = json!({"type": "ephemeral", "ttl": "1h"});
    assert_eq!(
        CacheControl::of(&json!({"cache_control": ttl})),
        Some(CacheControl(ttl))
    );
}

#[test]
fn every_placement_counts_as_a_mark() {
    let plain = request(vec![Message::text(Role::User, "hi")]);
    assert!(!has_marks(&plain));

    let mut top = plain.clone();
    top.cache_control = Some(json!({"type": "ephemeral"}));
    assert!(has_marks(&top));

    let mut tooled = plain.clone();
    tooled.tools = vec![tool(json!({"cache_control": {"type": "ephemeral"}}))];
    assert!(has_marks(&tooled));

    let mut unmarked_tool = plain.clone();
    unmarked_tool.tools = vec![tool(json!({"eager_input_streaming": true}))];
    assert!(!has_marks(&unmarked_tool));

    let marked_text = ContentPart::Text {
        text: "sys".into(),
        citations: Vec::new(),
        citations_scope: None,
        cache_control: mark(),
    };
    assert!(has_marks(&request(vec![Message {
        role: Role::System,
        content: vec![marked_text],
    }])));

    let nested = ContentPart::ToolResult {
        id: "c".into(),
        name: None,
        content: vec![ToolResultBlock::Text {
            text: "r".into(),
            cache_control: mark(),
        }],
        is_error: false,
        cache_control: None,
    };
    assert!(has_marks(&request(vec![Message {
        role: Role::Tool,
        content: vec![nested],
    }])));

    let verbatim = ContentPart::AnthropicBlock(crate::ir::AnthropicBlock::from_client(json!({
        "type": "document",
        "source": {"type": "text", "media_type": "text/plain", "data": "d"},
        "cache_control": {"type": "ephemeral"},
    })));
    assert!(has_marks(&request(vec![Message {
        role: Role::User,
        content: vec![verbatim],
    }])));
}

#[test]
fn off_an_anthropic_route_the_marks_and_tool_fields_are_named() {
    let mut ir = request(vec![Message::text(Role::User, "hi")]);
    ir.cache_control = Some(json!({"type": "ephemeral"}));
    ir.tools = vec![
        tool(json!({"cache_control": {"type": "ephemeral"}, "eager_input_streaming": true})),
        tool(json!({"eager_input_streaming": false, "bad key": 1})),
    ];
    let c = ReasoningControl::default();
    for proto in [
        Protocol::Openai,
        Protocol::OpenaiCompatible,
        Protocol::Gemini,
        Protocol::LlamaCpp,
    ] {
        assert_eq!(
            ir.fields_ignored(proto, &c),
            [
                "cache_control",
                "tools.eager_input_streaming",
                "tools.field"
            ]
        );
    }
    assert!(ir.fields_ignored(Protocol::Anthropic, &c).is_empty());

    let plain = request(vec![Message::text(Role::User, "hi")]);
    assert!(plain.fields_ignored(Protocol::Openai, &c).is_empty());
}

#[test]
fn a_system_message_joins_by_a_blank_line_every_other_by_a_line_break() {
    let parts = vec![ContentPart::text("a"), ContentPart::text("b")];
    let system = Message {
        role: Role::System,
        content: parts.clone(),
    };
    assert_eq!(system.joined_text(), "a\n\nb");
    let user = Message {
        role: Role::User,
        content: parts,
    };
    assert_eq!(user.joined_text(), "a\nb");
}

#[test]
fn an_openai_system_message_joins_its_parts_by_a_line_break_into_one() {
    let image = ContentPart::Image {
        mime: "image/png".into(),
        source: crate::ir::ImageSource::Url {
            url: "https://example.com/i.png".into(),
        },
        cache_control: None,
    };
    let m = Message {
        role: Role::System,
        content: vec![
            ContentPart::text("a"),
            image.clone(),
            ContentPart::text(""),
            ContentPart::text("b"),
        ],
    }
    .with_system_text_joined();
    assert_eq!(m.content, vec![ContentPart::text("a\n\nb"), image]);
    assert_eq!(m.joined_text(), "a\n\nb");

    // One part, and every other role, as they are.
    let one = Message::text(Role::System, "x").with_system_text_joined();
    assert_eq!(one, Message::text(Role::System, "x"));
    let user = Message {
        role: Role::User,
        content: vec![ContentPart::text("a"), ContentPart::text("b")],
    };
    assert_eq!(user.clone().with_system_text_joined(), user);
}
