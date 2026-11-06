//! Generated IR conversations (`proptest` strategies) for the property test:
//! text and system turns, images, tool calls with no, nested, array and
//! several arguments, stored arguments as `""`, `null` or unparseable text,
//! tool results with errors, JSON, resources and images, reasoning with and
//! without a signature, refusals, and turns that go on after the tools.
//!
//! A conversation is built the way an ingress leaves one: it starts with an
//! optional system message, alternates user and assistant turns, and every
//! assistant tool call is answered before the next user turn. Its tool
//! results come either as one `tool` message per call (the OpenAI shape) or
//! as one user message holding all of them (the Anthropic shape), since the
//! egresses must take both.

use lmgw_core::ir::{
    ChatRequest, ContentPart, ImageSource, Message, Params, Role, ToolChoice, ToolDef,
    ToolResultBlock,
};
use proptest::prelude::*;
use serde_json::{json, Value};

/// A tiny but real PNG's base64: one transparent pixel.
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

const TOOLS: [&str; 3] = ["get_weather", "list_files", "now"];

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("hello".to_string()),
        Just("Wie spät ist es? 🕑".to_string()),
        Just(String::new()),
        "[a-z ]{1,24}",
    ]
}

/// A tool call's arguments as the IR may hold them, stored threads included.
fn call_args() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(json!({})),
        Just(json!({"city": "Berlin"})),
        Just(json!({"filter": {"kind": "pdf", "deep": {"level": 2}}})),
        Just(json!({"paths": ["/a", "/b"], "limits": [1, 2, 3]})),
        Just(json!({"a": 1, "b": "two", "c": [true, null], "d": {"e": 1.5}})),
        // What a thread stored before `ir::parse_call_args` (d3fa11d1).
        Just(json!("")),
        Just(Value::Null),
        // A model's broken JSON, kept as the text it sent.
        Just(json!("{\"city\": \"Ber")),
        // A string holding an object, as some stores kept it.
        Just(json!("{\"city\":\"Berlin\"}")),
    ]
}

fn user_part() -> impl Strategy<Value = ContentPart> {
    prop_oneof![
        4 => text().prop_map(ContentPart::text),
        1 => Just(ContentPart::Image {
            mime: "image/png".into(),
            source: ImageSource::Base64 { data: PNG.into() },
        }),
        1 => Just(ContentPart::Image {
            mime: "image/jpeg".into(),
            source: ImageSource::Url { url: "https://example.com/cat.jpg".into() },
        }),
    ]
}

fn result_block() -> impl Strategy<Value = ToolResultBlock> {
    prop_oneof![
        4 => text().prop_map(ToolResultBlock::text),
        1 => Just(ToolResultBlock::Json { value: json!({"temp": 21, "unit": "C"}) }),
        1 => Just(ToolResultBlock::Image { mime: "image/png".into(), data: PNG.into() }),
        1 => Just(ToolResultBlock::Resource {
            uri: "file:///notes.txt".into(),
            mime: Some("text/plain".into()),
            text: Some("a note".into()),
        }),
        1 => Just(ToolResultBlock::Resource {
            uri: "file:///scan.pdf".into(),
            mime: Some("application/pdf".into()),
            text: None,
        }),
    ]
}

/// One assistant turn's leading parts: reasoning (signed or not), text, a
/// refusal.
fn assistant_parts() -> impl Strategy<Value = Vec<ContentPart>> {
    (
        prop::option::of((
            text(),
            prop::option::of(Just("sig-EqQBCkYIBRgCKkB".to_string())),
        )),
        prop::option::of(text()),
        prop::bool::weighted(0.1),
    )
        .prop_map(|(reasoning, said, refused)| {
            let mut parts = Vec::new();
            if let Some((t, signature)) = reasoning {
                parts.push(ContentPart::Reasoning {
                    text: t,
                    signature,
                    sealed: None,
                    signature_scope: None,
                });
            }
            if let Some(t) = said {
                parts.push(ContentPart::text(t));
            }
            if refused {
                parts.push(ContentPart::Refusal {
                    text: "I can't help with that.".into(),
                });
            }
            parts
        })
}

/// A call: which tool, its arguments, and its result (blocks, error).
type Call = (usize, Value, Vec<ToolResultBlock>, bool);

fn call() -> impl Strategy<Value = Call> {
    (
        0..TOOLS.len(),
        call_args(),
        prop::collection::vec(result_block(), 1..3),
        prop::bool::weighted(0.2),
    )
}

/// One exchange: the user's turn, then the assistant's, with its calls
/// answered.
type Exchange = (Vec<ContentPart>, Vec<ContentPart>, Vec<Call>, bool);

fn exchange() -> impl Strategy<Value = Exchange> {
    (
        prop::collection::vec(user_part(), 1..3),
        assistant_parts(),
        prop::collection::vec(call(), 0..4),
        any::<bool>(),
    )
}

/// A whole request.
pub fn conversation() -> impl Strategy<Value = ChatRequest> {
    (
        prop::option::of(text()),
        prop::collection::vec(exchange(), 1..4),
        prop::collection::vec(user_part(), 0..2),
        prop::option::of(0..4usize),
        prop::option::of(1u32..4096),
        prop::option::of(0.0f64..1.5),
    )
        .prop_map(
            |(system, exchanges, last, choice, max_tokens, temperature)| {
                build(system, exchanges, last, choice, max_tokens, temperature)
            },
        )
}

fn build(
    system: Option<String>,
    exchanges: Vec<Exchange>,
    last: Vec<ContentPart>,
    choice: Option<usize>,
    max_tokens: Option<u32>,
    temperature: Option<f64>,
) -> ChatRequest {
    let mut messages = Vec::new();
    if let Some(s) = system {
        messages.push(Message::text(Role::System, s));
    }
    let mut n = 0;
    let count = exchanges.len();
    for (i, (user, mut assistant, calls, one_message)) in exchanges.into_iter().enumerate() {
        messages.push(Message {
            role: Role::User,
            content: user,
        });
        // The last exchange's assistant turn is the one being asked for, so
        // it is left out, unless it made calls (whose results then end the
        // conversation, as a tool loop's next request does) or a closing
        // user turn follows.
        if i + 1 == count && calls.is_empty() && last.is_empty() {
            break;
        }
        let mut results = Vec::new();
        for (tool, args, blocks, is_error) in calls {
            n += 1;
            let id = format!("call_{n}");
            assistant.push(ContentPart::ToolUse {
                id: id.clone(),
                name: TOOLS[tool].into(),
                args,
            });
            results.push(ContentPart::ToolResult {
                id,
                name: Some(TOOLS[tool].into()),
                content: blocks,
                is_error,
            });
        }
        if assistant.is_empty() {
            assistant.push(ContentPart::text("ok"));
        }
        messages.push(Message {
            role: Role::Assistant,
            content: assistant,
        });
        if results.is_empty() {
            continue;
        }
        if one_message {
            messages.push(Message {
                role: Role::User,
                content: results,
            });
        } else {
            for r in results {
                messages.push(Message {
                    role: Role::Tool,
                    content: vec![r],
                });
            }
        }
    }
    if !last.is_empty() {
        messages.push(Message {
            role: Role::User,
            content: last,
        });
    }
    let tools = TOOLS
        .iter()
        .map(|name| ToolDef {
            name: (*name).into(),
            description: (name != &"now").then(|| format!("{name}, for the test")),
            parameters: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
            }),
            strict: Some(false),
        })
        .collect();
    let tool_choice = choice.map(|c| match c {
        0 => ToolChoice::Auto,
        1 => ToolChoice::None,
        2 => ToolChoice::Required,
        _ => ToolChoice::Tool {
            name: TOOLS[0].into(),
        },
    });
    ChatRequest {
        model_alias: "m".into(),
        messages,
        params: Params {
            max_tokens,
            temperature,
            ..Params::default()
        },
        tools,
        tool_choice,
        response_format: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
        context_management: None,
        compaction: None,
        thinking_display: None,
    }
}
