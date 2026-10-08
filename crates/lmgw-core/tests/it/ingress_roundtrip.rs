//! Ingress round-trips (§15): OpenAI-in and Anthropic-in → IR →
//! re-serialized out, plus stream-encoder framing.

use lmgw_core::ingress::{anthropic, openai, ClientProto};
use lmgw_core::ir::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// OpenAI parsing
// ---------------------------------------------------------------------------

#[test]
fn openai_parses_text_request() {
    let body = json!({
        "model": "my-model",
        "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "hi"}
        ],
        "temperature": 0.7,
        "max_tokens": 100,
        "stop": ["END"],
        "stream": true
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ir.model_alias, "my-model");
    assert_eq!(ir.messages.len(), 2);
    assert_eq!(ir.messages[0].role, Role::System);
    assert_eq!(ir.system_text().as_deref(), Some("be terse"));
    assert_eq!(ir.params.temperature, Some(0.7));
    assert_eq!(ir.params.max_tokens, Some(100));
    assert_eq!(ir.params.stop, vec!["END".to_string()]);
    assert!(ir.stream);
}

#[test]
fn openai_parses_tools_and_tool_results() {
    let body = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Berlin\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "12°C"}
        ],
        "tools": [{"type": "function", "function": {
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        }}],
        "tool_choice": "auto"
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ir.tools.len(), 1);
    assert_eq!(ir.tools[0].name, "get_weather");
    assert_eq!(ir.tool_choice, Some(ToolChoice::Auto));
    assert!(matches!(
        &ir.messages[1].content[0],
        ContentPart::ToolUse { id, name, args }
            if id == "call_1" && name == "get_weather" && args["city"] == "Berlin"
    ));
    assert!(matches!(
        &ir.messages[2].content[0],
        ContentPart::ToolResult { id, content, .. }
            if id == "call_1" && content == &ToolResultBlock::one("12°C")
    ));
    assert_eq!(ir.tool_name_for_id("call_1"), Some("get_weather"));
}

#[test]
fn openai_carries_unmodeled_fields_through_passthrough() {
    // `response_format` (both forms), `grammar`, and extra llama.cpp samplers
    // are not modeled in the IR. Without passthrough they were silently dropped
    // before reaching llama-server, so structured output was never constrained.
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.2,
        "response_format": {"type": "json_schema",
            "json_schema": {"name": "out", "schema": {"type": "object"}}},
        "typical_p": 0.05,
        "grammar": "root ::= \"x\""
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    // Modeled fields are consumed into the IR, never duplicated into passthrough.
    assert!(!ir.passthrough.contains_key("model"));
    assert!(!ir.passthrough.contains_key("messages"));
    assert!(!ir.passthrough.contains_key("temperature"));
    assert_eq!(ir.params.temperature, Some(0.2));
    // Unmodeled fields are captured verbatim.
    assert_eq!(ir.passthrough["response_format"]["type"], "json_schema");
    assert_eq!(
        ir.passthrough["response_format"]["json_schema"]["name"],
        "out"
    );
    assert_eq!(ir.passthrough["typical_p"], 0.05);
    assert_eq!(ir.passthrough["grammar"], "root ::= \"x\"");
}

#[test]
fn openai_models_min_p_and_repeat_penalty_and_llama_egress_emits_them_once() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "min_p": 0.05,
        "repeat_penalty": 1.1
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ir.params.min_p, Some(0.05));
    assert_eq!(ir.params.repeat_penalty, Some(1.1));
    // Modelled, so no longer carried by passthrough.
    assert!(!ir.passthrough.contains_key("min_p"));
    assert!(!ir.passthrough.contains_key("repeat_penalty"));
    let out = lmgw_core::egress::llama_cpp::chat_body(
        &ir,
        "up",
        &ir.params,
        false,
        &lmgw_core::config::Snapshot::default().router_upstream(),
    );
    assert_eq!(out["min_p"], 0.05);
    assert_eq!(out["repeat_penalty"], 1.1);
    let text = out.to_string();
    assert_eq!(text.matches("\"min_p\"").count(), 1);
    assert_eq!(text.matches("\"repeat_penalty\"").count(), 1);
}

#[test]
fn openai_parses_image_parts() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            {"type": "image_url", "image_url": {"url": "https://example.com/cat.jpg"}}
        ]}]
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    let parts = &ir.messages[0].content;
    assert!(
        matches!(&parts[1], ContentPart::Image { mime, source: ImageSource::Base64 { data } }
        if mime == "image/png" && data == "AAAA")
    );
    assert!(
        matches!(&parts[2], ContentPart::Image { mime, source: ImageSource::Url { url } }
        if mime == "image/jpeg" && url.ends_with("cat.jpg"))
    );
}

#[test]
fn openai_parses_input_audio_parts() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "listen"},
            {"type": "input_audio", "input_audio": {"data": "QUFB", "format": "WAV"}},
            {"type": "input_audio", "input_audio": {"data": "data:audio/mp3;base64,QkJC"}}
        ]}]
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    let parts = &ir.messages[0].content;
    // Raw base64 + `format` → mime is `audio/<format>`, lowercased.
    assert!(matches!(&parts[1], ContentPart::Audio { mime, data }
        if mime == "audio/wav" && data == "QUFB"));
    // A `data:` URI keeps its own mime; `format` isn't needed.
    assert!(matches!(&parts[2], ContentPart::Audio { mime, data }
        if mime == "audio/mp3" && data == "QkJC"));
}

#[test]
fn openai_input_audio_without_format_or_data_uri_is_a_bad_request() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "input_audio", "input_audio": {"data": "QUFB"}}
        ]}]
    });
    let err = openai::parse_chat_request(&body).unwrap_err();
    assert!(matches!(err, lmgw_core::error::GatewayError::BadRequest(_)));
}

#[test]
fn responses_parses_input_audio_part() {
    let body = json!({
        "model": "m",
        "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "listen"},
            {"type": "input_audio", "input_audio": {"data": "QUFB", "format": "WAV"}}
        ]}]
    });
    let req = lmgw_core::ingress::responses::parse_request(&body).unwrap();
    let parts = &req.ir.messages[0].content;
    assert!(matches!(&parts[1], ContentPart::Audio { mime, data }
        if mime == "audio/wav" && data == "QUFB"));
}

#[test]
fn openai_serializes_completion_with_tool_calls() {
    let c = Completion {
        content: vec![ContentPart::ToolUse {
            id: "call_9".into(),
            name: "f".into(),
            args: json!({"x": 1}),
        }],
        reasoning: String::new(),
        finish_reason: FinishReason::ToolUse,
        usage: Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            ..Default::default()
        },
        model: "upstream-name".into(),
        timings: None,
    };
    let v = openai::serialize_completion("alias-x", &c);
    assert_eq!(v["model"], "alias-x");
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert!(v["choices"][0]["message"]["content"].is_null());
    let tc = &v["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_9");
    assert_eq!(tc["function"]["arguments"], "{\"x\":1}");
    assert_eq!(v["usage"]["total_tokens"], 15);
}

// ---------------------------------------------------------------------------
// Anthropic parsing
// ---------------------------------------------------------------------------

#[test]
fn anthropic_parses_messages_request() {
    let body = json!({
        "model": "my-claude",
        "max_tokens": 256,
        "system": "be helpful",
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "look:"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "QUJD"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "I see."},
                {"type": "tool_use", "id": "tu_1", "name": "lookup", "input": {"q": "x"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "result!", "is_error": false}
            ]}
        ],
        "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "any"},
        "top_k": 40,
        "stop_sequences": ["STOP"]
    });
    let ir = anthropic::parse_messages_request(&body).unwrap();
    assert_eq!(ir.model_alias, "my-claude");
    assert_eq!(ir.system_text().as_deref(), Some("be helpful"));
    assert_eq!(ir.params.max_tokens, Some(256));
    assert_eq!(ir.params.top_k, Some(40));
    assert_eq!(ir.tool_choice, Some(ToolChoice::Required));
    // tool_result becomes a Role::Tool message
    let tool_msg = ir.messages.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(matches!(&tool_msg.content[0],
        ContentPart::ToolResult { id, content, .. }
            if id == "tu_1" && content == &ToolResultBlock::one("result!")));
    assert_eq!(ir.tool_name_for_id("tu_1"), Some("lookup"));
}

#[test]
fn anthropic_serializes_completion() {
    let c = Completion {
        content: vec![
            ContentPart::text("hello"),
            ContentPart::ToolUse {
                id: "tu_2".into(),
                name: "f".into(),
                args: json!({"a": true}),
            },
        ],
        reasoning: "let me think".into(),
        finish_reason: FinishReason::ToolUse,
        usage: Usage {
            prompt_tokens: Some(7),
            completion_tokens: Some(3),
            ..Default::default()
        },
        model: "x".into(),
        timings: None,
    };
    let v = anthropic::serialize_completion("alias-y", &c);
    assert_eq!(v["type"], "message");
    assert_eq!(v["model"], "alias-y");
    assert_eq!(v["stop_reason"], "tool_use");
    // Reasoning is a leading `thinking` block; answer blocks follow.
    assert_eq!(v["content"][0]["type"], "thinking");
    assert_eq!(v["content"][0]["thinking"], "let me think");
    assert_eq!(v["content"][1]["type"], "text");
    assert_eq!(v["content"][2]["type"], "tool_use");
    assert_eq!(v["content"][2]["input"]["a"], true);
    assert_eq!(v["usage"]["input_tokens"], 7);
    assert_eq!(v["usage"]["output_tokens"], 3);
}

// ---------------------------------------------------------------------------
// Stream encoders
// ---------------------------------------------------------------------------

fn collect_sse_events(raw: &str) -> Vec<(Option<String>, String)> {
    let mut dec = lmgw_core::sse::SseDecoder::new();
    dec.feed(raw.as_bytes())
        .into_iter()
        .map(|e| (e.event, e.data))
        .collect()
}

/// A two-token answer whose upstream reported usage before its stop, run
/// through the OpenAI encoder: the JSON chunks, and whether `[DONE]` ended it.
fn openai_stream_with_usage(include_usage: bool) -> (Vec<serde_json::Value>, bool) {
    let mut enc = ClientProto::OpenaiChat.new_stream_encoder("alias-z", include_usage);
    let mut out = String::new();
    out.push_str(&enc.start());
    out.push_str(&enc.delta(&StreamDelta::TextDelta("Hel".into())));
    out.push_str(&enc.delta(&StreamDelta::TextDelta("lo".into())));
    out.push_str(&enc.delta(&StreamDelta::Usage(Usage {
        prompt_tokens: Some(4),
        completion_tokens: Some(2),
        ..Default::default()
    })));
    out.push_str(&enc.delta(&StreamDelta::Stop(FinishReason::Stop)));
    out.push_str(&enc.finish());

    let events = collect_sse_events(&out);
    let done = events.last().is_some_and(|(_, d)| d == "[DONE]");
    let chunks = events
        .iter()
        .filter_map(|(_, d)| serde_json::from_str::<serde_json::Value>(d).ok())
        .collect();
    (chunks, done)
}

#[test]
fn openai_stream_encoder_frames() {
    let (chunks, done) = openai_stream_with_usage(true);
    assert!(done);
    let first = &chunks[0];
    assert_eq!(first["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(first["model"], "alias-z");
    let texts: Vec<String> = chunks
        .iter()
        .filter_map(|v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(String::from)
        })
        .collect();
    assert_eq!(texts.concat(), "Hello");
    // Asked for, the usage is the last chunk, with empty choices — and every
    // chunk before it says `usage: null`, as OpenAI's do.
    let (last, rest) = chunks.split_last().unwrap();
    assert_eq!(last["choices"], serde_json::json!([]));
    assert_eq!(last["usage"]["prompt_tokens"], 4);
    assert_eq!(last["usage"]["total_tokens"], 6);
    for c in rest {
        assert!(c["usage"].is_null() && c.get("usage").is_some(), "{c}");
        assert_eq!(c["choices"].as_array().map(Vec::len), Some(1), "{c}");
    }
}

/// Not asked for, the usage stays the gateway's: no chunk carries the key,
/// and none has empty `choices` — the SDK loop that reads `choices[0]` holds
/// to the last chunk.
#[test]
fn openai_stream_encoder_sends_no_usage_chunk_unasked() {
    let (chunks, done) = openai_stream_with_usage(false);
    assert!(done);
    assert_eq!(
        chunks.last().unwrap()["choices"][0]["finish_reason"],
        "stop"
    );
    for c in &chunks {
        assert!(c.get("usage").is_none(), "{c}");
        assert_eq!(c["choices"].as_array().map(Vec::len), Some(1), "{c}");
    }
}

#[test]
fn anthropic_stream_encoder_event_sequence() {
    let mut enc = ClientProto::AnthropicMessages.new_stream_encoder("alias-a", false);
    let mut out = String::new();
    out.push_str(&enc.start());
    out.push_str(&enc.delta(&StreamDelta::TextDelta("Hi".into())));
    out.push_str(&enc.delta(&StreamDelta::ToolCallStart {
        index: 0,
        id: "tu_5".into(),
        name: "f".into(),
    }));
    out.push_str(&enc.delta(&StreamDelta::ToolCallArgsDelta {
        index: 0,
        fragment: "{\"a\":".into(),
    }));
    out.push_str(&enc.delta(&StreamDelta::ToolCallArgsDelta {
        index: 0,
        fragment: "1}".into(),
    }));
    out.push_str(&enc.delta(&StreamDelta::Usage(Usage {
        prompt_tokens: Some(11),
        completion_tokens: Some(6),
        ..Default::default()
    })));
    out.push_str(&enc.delta(&StreamDelta::Stop(FinishReason::ToolUse)));
    out.push_str(&enc.finish());

    let names: Vec<String> = collect_sse_events(&out)
        .into_iter()
        .filter_map(|(e, _)| e)
        .collect();
    assert_eq!(
        names,
        [
            "message_start",
            "ping",
            "content_block_start", // text
            "content_block_delta",
            "content_block_stop",  // text closed by tool start
            "content_block_start", // tool_use
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );

    let events = collect_sse_events(&out);
    let md: serde_json::Value = serde_json::from_str(
        &events
            .iter()
            .find(|(e, _)| e.as_deref() == Some("message_delta"))
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(md["delta"]["stop_reason"], "tool_use");
    assert_eq!(md["usage"]["output_tokens"], 6);
    assert_eq!(md["usage"]["input_tokens"], 11);
}

#[test]
fn openai_stream_encoder_forwards_reasoning() {
    let mut enc = ClientProto::OpenaiChat.new_stream_encoder("alias-r", false);
    let mut out = String::new();
    out.push_str(&enc.start());
    out.push_str(&enc.delta(&StreamDelta::ReasoningDelta("Th".into())));
    out.push_str(&enc.delta(&StreamDelta::ReasoningDelta("ink".into())));
    out.push_str(&enc.delta(&StreamDelta::TextDelta("Hi".into())));
    out.push_str(&enc.delta(&StreamDelta::Stop(FinishReason::Stop)));
    out.push_str(&enc.finish());

    let chunks: Vec<serde_json::Value> = collect_sse_events(&out)
        .into_iter()
        .filter_map(|(_, d)| serde_json::from_str(&d).ok())
        .collect();
    let reasoning: String = chunks
        .iter()
        .filter_map(|v| v["choices"][0]["delta"]["reasoning_content"].as_str())
        .collect();
    let content: String = chunks
        .iter()
        .filter_map(|v| v["choices"][0]["delta"]["content"].as_str())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(reasoning, "Think");
    assert_eq!(content, "Hi");
}

#[test]
fn anthropic_stream_encoder_maps_reasoning_to_thinking() {
    let mut enc = ClientProto::AnthropicMessages.new_stream_encoder("alias-r", false);
    let mut out = String::new();
    out.push_str(&enc.start());
    out.push_str(&enc.delta(&StreamDelta::ReasoningDelta("pondering".into())));
    out.push_str(&enc.delta(&StreamDelta::TextDelta("answer".into())));
    out.push_str(&enc.delta(&StreamDelta::Stop(FinishReason::Stop)));
    out.push_str(&enc.finish());

    let events = collect_sse_events(&out);
    // Thinking block opens first, then closes when the text block begins.
    let names: Vec<String> = events.iter().filter_map(|(e, _)| e.clone()).collect();
    assert_eq!(
        names[2], "content_block_start",
        "thinking block opens: {names:?}"
    );
    let first_block: serde_json::Value = serde_json::from_str(&events[2].1).unwrap();
    assert_eq!(first_block["content_block"]["type"], "thinking");
    // A thinking_delta carries the reasoning text.
    let td = events
        .iter()
        .filter_map(|(e, d)| (e.as_deref() == Some("content_block_delta")).then_some(d))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .find(|v| v["delta"]["type"] == "thinking_delta")
        .expect("a thinking_delta event");
    assert_eq!(td["delta"]["thinking"], "pondering");
}

// ---------------------------------------------------------------------------
// Reasoning replay: an assistant turn's trace survives ingress
// ---------------------------------------------------------------------------

#[test]
fn openai_keeps_replayed_reasoning_on_assistant_turns() {
    let body = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "pick a colour"},
            {"role": "assistant", "content": "Teal", "reasoning_content": "teal is unusual"},
            {"role": "user", "content": "why?"}
        ]
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(
        ir.messages[1].content,
        vec![
            ContentPart::reasoning("teal is unusual"),
            ContentPart::text("Teal")
        ]
    );
    assert_eq!(ir.messages[1].reasoning_text(), "teal is unusual");

    // OpenRouter's spelling is the same field.
    let body = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "pick a colour"},
            {"role": "assistant", "content": "Teal", "reasoning": "teal is unusual"}
        ]
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ir.messages[1].reasoning_text(), "teal is unusual");

    // An empty trace is no trace: a plain turn stays a plain turn.
    let body = json!({
        "model": "m",
        "messages": [{"role": "assistant", "content": "Teal", "reasoning_content": ""}]
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ir.messages[0].content, vec![ContentPart::text("Teal")]);
}

#[test]
fn anthropic_keeps_thinking_blocks_with_their_signature() {
    let body = json!({
        "model": "m",
        "max_tokens": 10,
        "messages": [
            {"role": "user", "content": "pick a colour"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "teal is unusual", "signature": "sig-1"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "Teal"}
            ]},
            {"role": "user", "content": "why?"}
        ]
    });
    let ir = anthropic::parse_messages_request(&body).unwrap();
    assert_eq!(
        ir.messages[1].content,
        vec![
            ContentPart::Reasoning {
                text: "teal is unusual".into(),
                signature: Some("sig-1".into()),
            },
            ContentPart::text("Teal"),
        ]
    );

    // The gateway's own unsigned thinking block — what `serialize_completion`
    // hands out for a llama.cpp trace — comes back as an unsigned part.
    let body = json!({
        "model": "m",
        "max_tokens": 10,
        "messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "ok"}
        ]}]
    });
    let ir = anthropic::parse_messages_request(&body).unwrap();
    assert_eq!(ir.messages[0].content[0], ContentPart::reasoning("hmm"));
}

// ---------------------------------------------------------------------------
// Reasoning control sources (model-capabilities design §5.2)
// ---------------------------------------------------------------------------

fn ctl(ir: &ChatRequest) -> ReasoningControl {
    ir.params.reasoning.clone().unwrap_or_default()
}

#[test]
fn openai_reads_reasoning_effort() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ctl(&ir).effort.as_deref(), Some("high"));
    // Modeled, so the egress decides how to spell it — it does not also ride
    // along verbatim.
    assert!(!ir.passthrough.contains_key("reasoning_effort"));
}

/// Both spellings are read as a control **and** left in passthrough: the
/// client's own choice of key is what its upstream may already understand, and
/// deleting it would be lmgw picking a spelling on the client's behalf.
#[test]
fn openai_reads_budget_in_both_spellings_without_consuming_them() {
    for key in ["reasoning_budget_tokens", "thinking_budget_tokens"] {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            key: 2048,
        });
        let ir = openai::parse_chat_request(&body).unwrap();
        assert_eq!(ctl(&ir).budget_tokens, Some(2048), "{key}");
        assert_eq!(ir.passthrough[key], 2048, "{key}");
    }
}

/// A key is modeled only when it carries the modeled *type*. A
/// `reasoning_effort` that is null or a number is no level lmgw can resolve —
/// and swallowing it would delete a field the upstream might have understood.
#[test]
fn openai_keeps_an_unparseable_reasoning_effort_in_passthrough() {
    for bad in [json!(null), json!(3), json!({"level": "high"})] {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": bad,
        });
        let ir = openai::parse_chat_request(&body).unwrap();
        assert_eq!(ir.params.reasoning, None, "{bad}");
        assert_eq!(ir.passthrough["reasoning_effort"], bad, "{bad}");
    }
    // A budget that is not an integer is left alone the same way.
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_budget_tokens": "lots",
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ir.params.reasoning, None);
    assert_eq!(ir.passthrough["reasoning_budget_tokens"], "lots");
}

/// The OpenRouter-shaped object is *read* for control and still forwarded: it
/// carries `exclude` / `max_tokens`, which an OpenRouter-style upstream
/// understands and lmgw does not model.
#[test]
fn openai_reads_openrouter_reasoning_object_and_keeps_it_in_passthrough() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning": {"effort": "low", "max_tokens": 4096, "exclude": true},
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    let c = ctl(&ir);
    assert_eq!(c.effort.as_deref(), Some("low"));
    assert_eq!(c.budget_tokens, Some(4096));
    assert_eq!(ir.passthrough["reasoning"]["exclude"], true);
    assert_eq!(ir.passthrough["reasoning"]["max_tokens"], 4096);
}

#[test]
fn openai_reads_openrouter_reasoning_disabled() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning": {"enabled": false},
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ctl(&ir).enabled, Some(false));
    assert_eq!(ir.passthrough["reasoning"]["enabled"], false);
}

/// Both spellings in one body: the scalar key is what an OpenAI client sends,
/// so it is the one that wins.
#[test]
fn openai_scalar_effort_beats_the_object() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning": {"effort": "low"},
        "reasoning_effort": "high",
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ctl(&ir).effort.as_deref(), Some("high"));
}

/// `enable_thinking` names a variable of a llama.cpp chat template, so it is
/// **not** a protocol-neutral control: it lands in its own field and only
/// becomes one when the route turns out to render a template (§5.2).
#[test]
fn openai_reads_chat_template_kwargs_enable_thinking_separately() {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "chat_template_kwargs": {"enable_thinking": false, "custom": "keep me"},
    });
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ir.llama_kwargs_enabled, Some(false));
    assert_eq!(ir.params.reasoning, None, "not a protocol-neutral control");
    // Read, not consumed: the object carries template kwargs lmgw knows
    // nothing about and must still forward.
    assert_eq!(ir.passthrough["chat_template_kwargs"]["custom"], "keep me");
}

#[test]
fn openai_without_reasoning_keys_has_no_control() {
    let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    let ir = openai::parse_chat_request(&body).unwrap();
    assert_eq!(ir.params.reasoning, None);
}

#[test]
fn anthropic_reads_thinking_enabled_with_budget() {
    let body = json!({
        "model": "m", "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "enabled", "budget_tokens": 8000},
    });
    let ir = anthropic::parse_messages_request(&body).unwrap();
    let c = ctl(&ir);
    assert_eq!(c.enabled, Some(true));
    assert_eq!(c.budget_tokens, Some(8000));
}

#[test]
fn anthropic_reads_thinking_adaptive_and_disabled() {
    let adaptive = json!({
        "model": "m", "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "adaptive"},
    });
    let c = ctl(&anthropic::parse_messages_request(&adaptive).unwrap());
    assert_eq!(c.enabled, Some(true));
    assert_eq!(c.budget_tokens, None);

    let disabled = json!({
        "model": "m", "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "disabled"},
    });
    let c = ctl(&anthropic::parse_messages_request(&disabled).unwrap());
    assert_eq!(c.enabled, Some(false));
}

#[test]
fn anthropic_reads_output_config_effort() {
    let body = json!({
        "model": "m", "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "high"},
    });
    let c = ctl(&anthropic::parse_messages_request(&body).unwrap());
    assert_eq!(c.effort.as_deref(), Some("high"));
    assert_eq!(c.enabled, None);
}

#[test]
fn responses_reads_reasoning_effort() {
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"effort": "medium", "summary": "auto"},
    });
    let req = lmgw_core::ingress::responses::parse_request(&body).unwrap();
    assert_eq!(
        req.ir.params.reasoning.unwrap().effort.as_deref(),
        Some("medium")
    );
}

/// The control merges field-wise across tiers and collapses to a triple no
/// egress cell can contradict (§5.1).
#[test]
fn reasoning_control_merges_and_normalises() {
    let client = ReasoningControl {
        enabled: Some(true),
        ..Default::default()
    };
    let alias = ReasoningControl {
        effort: Some("low".into()),
        budget_tokens: Some(512),
        ..Default::default()
    };
    let merged = client.merge_over(Some(alias));
    assert_eq!(merged.enabled, Some(true));
    assert_eq!(merged.effort.as_deref(), Some("low"));
    assert_eq!(merged.budget_tokens, Some(512));

    // effort "none" is a disable, not a level.
    let n = ReasoningControl {
        effort: Some("none".into()),
        ..Default::default()
    }
    .normalised();
    assert_eq!(n.enabled, Some(false));
    assert_eq!(n.effort, None);

    // A zero budget is a disable too, and a disable clears everything else.
    let n = ReasoningControl {
        enabled: Some(true),
        effort: Some("high".into()),
        budget_tokens: Some(0),
    }
    .normalised();
    assert_eq!(n.enabled, Some(false));
    assert_eq!(n.effort, None);
    assert_eq!(n.budget_tokens, None);

    // Idempotent: an egress may re-apply it.
    assert_eq!(n.clone().normalised(), n);
}

/// Alias defaults fill only the gaps the client left (§5.1).
#[test]
fn params_with_defaults_merges_reasoning_field_wise() {
    let client = Params {
        reasoning: Some(ReasoningControl {
            enabled: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let defaults = Params {
        reasoning: Some(ReasoningControl {
            effort: Some("xhigh".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = client.with_defaults(&defaults);
    let c = merged.reasoning.unwrap();
    assert_eq!(c.enabled, Some(true));
    assert_eq!(c.effort.as_deref(), Some("xhigh"));
}

/// A lower tier's "off" must not survive to erase a higher tier's level.
///
/// Normalisation runs per tier, and a real level carries its own `enabled`, so
/// the tier that asked for `high` outranks the alias that said `enabled: false`
/// — rather than the two being merged field by field into "off, at high
/// effort", which then collapses to plain "off".
#[test]
fn a_higher_tiers_effort_overrides_a_lower_tiers_off() {
    let alias_off = Params {
        reasoning: Some(ReasoningControl {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let body_low = Params {
        reasoning: Some(ReasoningControl {
            effort: Some("low".into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let merged = body_low.clone().with_defaults(&alias_off);
    let c = merged.reasoning_control();
    assert_eq!(c.enabled, Some(true));
    assert_eq!(c.effort.as_deref(), Some("low"));

    // And the other way round: an alias that sets a level does not resurrect
    // thinking the *client* switched off.
    let alias_high = Params {
        reasoning: Some(ReasoningControl {
            effort: Some("high".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let body_off = Params {
        reasoning: Some(ReasoningControl {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let c = body_off.with_defaults(&alias_high).reasoning_control();
    assert_eq!(c.enabled, Some(false));
    assert_eq!(c.effort, None);
}

/// A positive budget implies thinking is on for the same reason a level does.
#[test]
fn a_budget_carries_its_own_enabled() {
    let c = ReasoningControl {
        budget_tokens: Some(2048),
        ..Default::default()
    }
    .normalised();
    assert_eq!(c.enabled, Some(true));
    assert_eq!(c.budget_tokens, Some(2048));

    // Zero is still the off switch, not a budget.
    let c = ReasoningControl {
        budget_tokens: Some(0),
        ..Default::default()
    }
    .normalised();
    assert_eq!(c.enabled, Some(false));
    assert_eq!(c.budget_tokens, None);
}

// ---------------------------------------------------------------------------
// Usage detail round-trips (usage-analytics §2.3)
//
// The IR's `prompt_tokens` is the total input *including* cache, because the
// Anthropic adapter sums that provider's three disjoint counters into it. So
// the Anthropic ingress has to undo the sum on the way out, or a client talking
// Anthropic-to-Anthropic through the gateway sees an inflated `input_tokens`
// and no cache fields at all.
// ---------------------------------------------------------------------------

fn cached_usage() -> Usage {
    Usage {
        // 100 plain + 800 read + 100 written
        prompt_tokens: Some(1000),
        completion_tokens: Some(42),
        cached_input_tokens: Some(800),
        cache_write_tokens: Some(100),
        reasoning_tokens: None,
    }
}

fn completion_with(usage: Usage) -> Completion {
    Completion {
        content: vec![ContentPart::text("hi")],
        reasoning: String::new(),
        finish_reason: FinishReason::Stop,
        usage,
        model: "m".into(),
        timings: None,
    }
}

#[test]
fn anthropic_ingress_undoes_the_sum() {
    let v = anthropic::serialize_completion("alias", &completion_with(cached_usage()));
    let u = &v["usage"];
    assert_eq!(u["input_tokens"], 100, "the plain remainder, not the total");
    assert_eq!(u["cache_read_input_tokens"], 800);
    assert_eq!(u["cache_creation_input_tokens"], 100);
    assert_eq!(u["output_tokens"], 42);
    // 100 + 800 + 100 is what the egress summed into the IR: exact round trip.
    assert_eq!(
        u["input_tokens"].as_u64().unwrap()
            + u["cache_read_input_tokens"].as_u64().unwrap()
            + u["cache_creation_input_tokens"].as_u64().unwrap(),
        1000
    );
}

#[test]
fn anthropic_ingress_omits_cache_fields_when_none_were_reported() {
    // An absent counter and a zero one are different statements.
    let u = Usage {
        prompt_tokens: Some(31),
        completion_tokens: Some(9),
        ..Default::default()
    };
    let v = anthropic::serialize_completion("alias", &completion_with(u));
    assert_eq!(v["usage"]["input_tokens"], 31);
    assert!(v["usage"].get("cache_read_input_tokens").is_none());
    assert!(v["usage"].get("cache_creation_input_tokens").is_none());
}

#[test]
fn openai_ingress_publishes_the_detail_objects() {
    let mut u = cached_usage();
    u.reasoning_tokens = Some(7);
    let v = openai::serialize_completion("alias", &completion_with(u));
    let usage = &v["usage"];
    assert_eq!(
        usage["prompt_tokens"], 1000,
        "the total, OpenAI's own meaning"
    );
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 800);
    assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 7);
}
