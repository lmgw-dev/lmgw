//! Chat bodies (§9.1): messages and history, tools and every `tool_choice`,
//! every kind of tool result, reasoning replay, user media, every sampler
//! field, stream on and off, and the passthrough fields llama.cpp reads.

use lmgw_core::ir::{ChatRequest, Params, ToolChoice, ToolDef};
use serde_json::{json, Value};

use super::{chat_request, ir, keyless, msg, params, Case};

/// A whole 1x1 PNG (`tool_result_image_png_whole`).
const WHOLE_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// A question, the assistant's call of `get_weather`, and its result made of
/// `blocks` (IR `ToolResultBlock`s).
fn tool_turn(blocks: Value, is_error: bool) -> ChatRequest {
    ir(json!([
        msg("user", "What is the weather in Berlin?"),
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "call_1", "name": "get_weather", "args": {"city": "Berlin"}},
        ]},
        {"role": "tool", "content": [
            {"type": "tool_result", "id": "call_1", "content": blocks, "is_error": is_error},
        ]},
    ]))
}

fn tools() -> Vec<ToolDef> {
    serde_json::from_value(json!([
        {"name": "get_weather", "description": "The weather in a city.",
         "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
                        "required": ["city"]}},
        {"name": "no_description", "parameters": {"type": "object", "properties": {}}},
    ]))
    .unwrap()
}

fn with_tools(choice: Option<Value>) -> ChatRequest {
    let mut r = ir(json!([msg("user", "Weather in Berlin?")]));
    r.tools = tools();
    r.tool_choice = choice.map(|c| serde_json::from_value::<ToolChoice>(c).unwrap());
    r
}

fn with_passthrough(mut r: ChatRequest, extra: Value) -> ChatRequest {
    r.passthrough = extra.as_object().cloned().unwrap();
    r
}

fn hello() -> ChatRequest {
    ir(json!([msg("user", "hello")]))
}

fn whole(r: ChatRequest) -> impl Fn(&lmgw_core::config::Upstream) -> Value {
    move |up| chat_request(up, &r, &Params::default(), false)
}

/// Every sampler field the IR models.
fn all_samplers() -> Params {
    params(json!({
        "temperature": 0.7, "top_p": 0.9, "top_k": 40, "min_p": 0.05,
        "repeat_penalty": 1.1, "max_tokens": 256, "presence_penalty": 0.5,
        "frequency_penalty": -0.5, "seed": 42, "stop": ["END", "\n\n"],
    }))
}

pub(super) fn cases() -> Vec<Case> {
    vec![
        // --- Text, system and history -------------------------------------
        Case {
            name: "text_whole",
            run: |up| whole(hello())(up),
        },
        Case {
            name: "text_stream",
            run: |up| chat_request(up, &hello(), &Params::default(), true),
        },
        Case {
            name: "text_keyless",
            run: |up| whole(hello())(&keyless(up)),
        },
        Case {
            name: "system_and_history",
            run: |up| {
                whole(ir(json!([
                    msg("system", "Be brief."),
                    msg("user", "hi"),
                    msg("assistant", "Hello! How can I help?"),
                    msg("user", "Tell me a joke."),
                ])))(up)
            },
        },
        Case {
            name: "user_text_parts",
            run: |up| {
                whole(ir(json!([{"role": "user", "content": [
                    {"type": "text", "text": "first part"},
                    {"type": "text", "text": "second part"},
                ]}])))(up)
            },
        },
        // --- Tools and every tool_choice ----------------------------------
        Case {
            name: "tools_choice_absent",
            run: |up| whole(with_tools(None))(up),
        },
        Case {
            name: "tools_choice_auto",
            run: |up| whole(with_tools(Some(json!({"type": "auto"}))))(up),
        },
        Case {
            name: "tools_choice_none",
            run: |up| whole(with_tools(Some(json!({"type": "none"}))))(up),
        },
        Case {
            name: "tools_choice_required",
            run: |up| whole(with_tools(Some(json!({"type": "required"}))))(up),
        },
        Case {
            name: "tools_choice_named",
            run: |up| {
                whole(with_tools(Some(
                    json!({"type": "tool", "name": "get_weather"}),
                )))(up)
            },
        },
        Case {
            name: "tool_choice_without_tools",
            run: |up| {
                let mut r = hello();
                r.tool_choice = Some(ToolChoice::Required);
                whole(r)(up)
            },
        },
        Case {
            name: "tools_stream",
            run: |up| {
                chat_request(
                    up,
                    &with_tools(Some(json!({"type": "auto"}))),
                    &Params::default(),
                    true,
                )
            },
        },
        // --- Tool calls and tool results ----------------------------------
        Case {
            name: "tool_call_with_text",
            run: |up| {
                whole(ir(json!([
                    msg("user", "Weather in Berlin?"),
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "Let me check."},
                        {"type": "tool_use", "id": "call_1", "name": "get_weather",
                         "args": {"city": "Berlin"}},
                    ]},
                    {"role": "tool", "content": [
                        {"type": "tool_result", "id": "call_1",
                         "content": [{"type": "text", "text": "12°C"}], "is_error": false},
                    ]},
                ])))(up)
            },
        },
        Case {
            name: "tool_calls_parallel",
            run: |up| {
                whole(ir(json!([
                    msg("user", "Weather in Berlin and Paris?"),
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "call_1", "name": "get_weather",
                         "args": {"city": "Berlin"}},
                        {"type": "tool_use", "id": "call_2", "name": "get_weather",
                         "args": {"city": "Paris"}},
                    ]},
                    {"role": "tool", "content": [
                        {"type": "tool_result", "id": "call_1", "name": "get_weather",
                         "content": [{"type": "text", "text": "12°C"}], "is_error": false},
                        {"type": "tool_result", "id": "call_2",
                         "content": [{"type": "text", "text": "15°C"}], "is_error": false},
                    ]},
                ])))(up)
            },
        },
        Case {
            name: "tool_result_text",
            run: |up| {
                whole(tool_turn(
                    json!([{"type": "text", "text": "12°C, sunny"}]),
                    false,
                ))(up)
            },
        },
        Case {
            name: "tool_result_json",
            run: |up| {
                whole(tool_turn(
                    json!([{"type": "json", "value": {"temp": 12, "unit": "C", "sky": ["sun"]}}]),
                    false,
                ))(up)
            },
        },
        Case {
            name: "tool_result_resource",
            run: |up| {
                whole(tool_turn(
                    json!([
                        {"type": "resource", "uri": "file:///notes.md", "mime": "text/markdown",
                         "text": "# Notes\n12°C"},
                        {"type": "resource", "uri": "file:///chart.pdf", "mime": "application/pdf"},
                        {"type": "resource", "uri": "file:///blob"},
                    ]),
                    false,
                ))(up)
            },
        },
        Case {
            name: "tool_result_image_png",
            run: |up| {
                whole(tool_turn(
                    json!([{"type": "image", "mime": "image/png", "data": "iVBORw0KGgo="}]),
                    false,
                ))(up)
            },
        },
        // A whole 1x1 PNG, which stb_image decodes: the one tool image the
        // llama upstream sends as an image. The case above carries the PNG
        // signature alone, which it cannot decode.
        Case {
            name: "tool_result_image_png_whole",
            run: |up| {
                whole(tool_turn(
                    json!([{"type": "image", "mime": "image/png", "data": WHOLE_PNG}]),
                    false,
                ))(up)
            },
        },
        Case {
            name: "tool_result_image_svg",
            run: |up| {
                whole(tool_turn(
                    json!([{"type": "image", "mime": "image/svg+xml", "data": "PHN2Zy8+"}]),
                    false,
                ))(up)
            },
        },
        Case {
            name: "tool_result_audio",
            run: |up| {
                whole(tool_turn(
                    json!([{"type": "audio", "mime": "audio/wav", "data": "UklGRg=="}]),
                    false,
                ))(up)
            },
        },
        Case {
            name: "tool_result_mixed",
            run: |up| {
                whole(tool_turn(
                    json!([
                        {"type": "text", "text": "Here is the chart:"},
                        {"type": "image", "mime": "image/png", "data": "iVBORw0KGgo="},
                        {"type": "json", "value": {"points": 3}},
                    ]),
                    false,
                ))(up)
            },
        },
        Case {
            name: "tool_result_error",
            run: |up| {
                whole(tool_turn(
                    json!([{"type": "text", "text": "city not found"}]),
                    true,
                ))(up)
            },
        },
        Case {
            name: "tool_result_empty",
            run: |up| whole(tool_turn(json!([]), false))(up),
        },
        // --- Reasoning replay ---------------------------------------------
        Case {
            name: "reasoning_replay",
            run: |up| {
                whole(ir(json!([
                    msg("user", "2+2?"),
                    {"role": "assistant", "content": [
                        {"type": "reasoning", "text": "Simple sum."},
                        {"type": "reasoning", "text": "It is four.", "signature": "sig"},
                        {"type": "text", "text": "4"},
                    ]},
                    msg("user", "And 3+3?"),
                ])))(up)
            },
        },
        Case {
            name: "reasoning_replay_tool_call",
            run: |up| {
                whole(ir(json!([
                    msg("user", "Weather in Berlin?"),
                    {"role": "assistant", "content": [
                        {"type": "reasoning", "text": "I need the tool."},
                        {"type": "tool_use", "id": "call_1", "name": "get_weather",
                         "args": {"city": "Berlin"}},
                    ]},
                    {"role": "tool", "content": [
                        {"type": "tool_result", "id": "call_1",
                         "content": [{"type": "text", "text": "12°C"}], "is_error": false},
                    ]},
                ])))(up)
            },
        },
        // --- User media ---------------------------------------------------
        Case {
            name: "user_image_url",
            run: |up| {
                whole(ir(json!([{"role": "user", "content": [
                    {"type": "text", "text": "What is this?"},
                    {"type": "image", "mime": "image/jpeg",
                     "source": {"type": "url", "url": "https://img.test/cat.jpg"}},
                ]}])))(up)
            },
        },
        Case {
            name: "user_image_base64",
            run: |up| {
                whole(ir(json!([{"role": "user", "content": [
                    {"type": "image", "mime": "image/png",
                     "source": {"type": "base64", "data": "iVBORw0KGgo="}},
                    {"type": "text", "text": "What is this?"},
                ]}])))(up)
            },
        },
        Case {
            name: "user_audio",
            run: |up| {
                whole(ir(json!([{"role": "user", "content": [
                    {"type": "audio", "mime": "audio/wav", "data": "UklGRg=="},
                ]}])))(up)
            },
        },
        Case {
            name: "user_mixed_media",
            run: |up| {
                whole(ir(json!([{"role": "user", "content": [
                    {"type": "text", "text": "Compare these."},
                    {"type": "image", "mime": "image/png",
                     "source": {"type": "base64", "data": "iVBORw0KGgo="}},
                    {"type": "audio", "mime": "audio/mpeg", "data": "SUQz"},
                    {"type": "audio", "mime": "wav", "data": "UklGRg=="},
                ]}])))(up)
            },
        },
        // --- Samplers, stop, seed, stream ---------------------------------
        Case {
            name: "samplers_whole",
            run: |up| chat_request(up, &hello(), &all_samplers(), false),
        },
        Case {
            name: "samplers_stream",
            run: |up| chat_request(up, &hello(), &all_samplers(), true),
        },
        Case {
            name: "samplers_edges",
            run: |up| {
                let p = params(json!({
                    "temperature": 0.0, "top_p": 1.0, "max_tokens": 1, "seed": -1,
                    "stop": ["</s>"],
                }));
                chat_request(up, &hello(), &p, false)
            },
        },
        // --- Passthrough ---------------------------------------------------
        Case {
            name: "passthrough_grammar",
            run: |up| {
                whole(with_passthrough(
                    hello(),
                    json!({"grammar": "root ::= \"yes\" | \"no\""}),
                ))(up)
            },
        },
        Case {
            name: "passthrough_response_format",
            run: |up| {
                whole(with_passthrough(
                    hello(),
                    json!({"response_format": {"type": "json_schema", "json_schema": {
                        "name": "answer", "strict": true,
                        "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
                    }}}),
                ))(up)
            },
        },
        Case {
            name: "passthrough_n_predict",
            run: |up| {
                let p = params(json!({"max_tokens": 32}));
                chat_request(
                    up,
                    &with_passthrough(hello(), json!({"n_predict": 64})),
                    &p,
                    false,
                )
            },
        },
        Case {
            name: "passthrough_timings_per_token",
            run: |up| {
                chat_request(
                    up,
                    &with_passthrough(hello(), json!({"timings_per_token": true})),
                    &Params::default(),
                    true,
                )
            },
        },
        Case {
            name: "passthrough_continue_final_message",
            run: |up| {
                whole(with_passthrough(
                    ir(json!([
                        msg("user", "Tell me a story."),
                        msg("assistant", "Once upon a"),
                    ])),
                    json!({"continue_final_message": true, "add_generation_prompt": false}),
                ))(up)
            },
        },
        Case {
            name: "passthrough_llama_samplers",
            run: |up| {
                whole(with_passthrough(
                    hello(),
                    json!({"typical_p": 0.9, "mirostat": 2, "cache_prompt": true,
                           "n_probs": 3, "dry_multiplier": 0.8}),
                ))(up)
            },
        },
        Case {
            name: "passthrough_loses_to_modelled",
            run: |up| {
                let p = params(json!({"temperature": 0.2}));
                chat_request(
                    up,
                    &with_passthrough(
                        hello(),
                        json!({"temperature": 1.5, "max_tokens": 9, "stream": false,
                               "model": "other", "messages": [],
                               "stream_options": {"include_usage": false}}),
                    ),
                    &p,
                    true,
                )
            },
        },
    ]
}
