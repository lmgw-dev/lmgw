//! The Responses body, piece by piece (openai egress design §7.2.2).

use serde_json::{json, Value};

use super::responses_body;
use crate::ir::{ChatRequest, Params};

/// The upstream row the bodies are sent to.
/// The row the tests' seals come from, as configured.
fn up() -> crate::ir::SealScope {
    crate::ir::SealScope::new(7, "cfg7")
}

/// An OpenAI chat body as the IR the chat ingress reads it into.
fn chat(v: Value) -> ChatRequest {
    crate::ingress::openai::parse_chat_request(&v).expect("a chat body")
}

/// The Responses body of `v`, sent with the request's own params.
fn body(v: Value, stream: bool) -> Value {
    let ir = chat(v);
    responses_body(&ir, "gpt-x", &ir.params, stream, &up())
}

#[test]
fn a_conversation_keeps_its_order_and_the_leading_system_text_is_instructions() {
    let b = body(
        json!({"model": "a", "messages": [
            {"role": "system", "content": "Be brief."},
            {"role": "system", "content": "Answer in English."},
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello."},
            {"role": "system", "content": "Now be formal."},
            {"role": "user", "content": "Again"},
        ]}),
        false,
    );
    assert_eq!(
        b,
        json!({
            "model": "gpt-x",
            "instructions": "Be brief.\n\nAnswer in English.",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "Hi"}]},
                {"role": "assistant", "content": "Hello."},
                {"type": "message", "role": "developer",
                 "content": [{"type": "input_text", "text": "Now be formal."}]},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "Again"}]},
            ],
            "reasoning": {"summary": "auto"},
            "store": false,
            "service_tier": "auto",
        })
    );
}

#[test]
fn tools_calls_and_their_results() {
    let b = body(
        json!({"model": "a", "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
               "parallel_tool_calls": false,
               "messages": [
            {"role": "user", "content": "Weather in Oslo and Bergen?"},
            {"role": "assistant", "content": "Looking.", "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}},
                {"id": "call_2", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Bergen\"}"}},
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "4 °C"},
            {"role": "tool", "tool_call_id": "call_2", "content": "7 °C"},
        ], "tools": [
            {"type": "function", "function": {"name": "get_weather",
             "description": "Weather by city", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "no_description",
             "parameters": {"type": "object", "properties": {}}}},
        ]}),
        false,
    );
    assert_eq!(
        b["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Weather in Oslo and Bergen?"}]},
            {"role": "assistant", "content": "Looking."},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather",
             "arguments": "{\"city\":\"Oslo\"}"},
            {"type": "function_call", "call_id": "call_2", "name": "get_weather",
             "arguments": "{\"city\":\"Bergen\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "4 °C"},
            {"type": "function_call_output", "call_id": "call_2", "output": "7 °C"},
        ])
    );
    assert_eq!(
        b["tools"],
        json!([
            {"type": "function", "name": "get_weather", "description": "Weather by city",
             "parameters": {"type": "object"}, "strict": false},
            {"type": "function", "name": "no_description",
             "parameters": {"type": "object", "properties": {}}, "strict": false},
        ])
    );
    assert_eq!(
        b["tool_choice"],
        json!({"type": "function", "name": "get_weather"})
    );
    assert_eq!(b["parallel_tool_calls"], json!(false));
}

#[test]
fn every_tool_choice_and_none_without_tools() {
    for (choice, want) in [
        (json!("auto"), json!("auto")),
        (json!("none"), json!("none")),
        (json!("required"), json!("required")),
    ] {
        let b = body(
            json!({"model": "a", "tool_choice": choice, "messages": [{"role": "user", "content": "x"}],
                   "tools": [{"type": "function", "function": {"name": "t", "parameters": {}}}]}),
            false,
        );
        assert_eq!(b["tool_choice"], want);
    }
    let b = body(
        json!({"model": "a", "tool_choice": "auto", "messages": [{"role": "user", "content": "x"}]}),
        false,
    );
    assert!(b.get("tools").is_none() && b.get("tool_choice").is_none());
}

/// A tool result with an image goes in the array form, the image as an
/// `input_image`; one without stays a string, and audio is the Chat wire's
/// placeholder text.
#[test]
fn a_tool_image_is_an_image_not_a_placeholder() {
    let ir: ChatRequest = serde_json::from_value(json!({
        "model_alias": "a",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "c1", "name": "shot", "args": {}}]},
            {"role": "tool", "content": [{"type": "tool_result", "id": "c1", "is_error": false,
             "content": [
                {"type": "text", "text": "The chart:"},
                {"type": "image", "mime": "image/png", "data": "iVBORw0KGgo="},
                {"type": "audio", "mime": "audio/wav", "data": "UklGRg=="},
             ]}]},
        ],
    }))
    .unwrap();
    let b = responses_body(&ir, "gpt-x", &Params::default(), false, &up());
    assert_eq!(
        b["input"][1],
        json!({"type": "function_call_output", "call_id": "c1", "output": [
            {"type": "input_text", "text": "The chart:"},
            {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo=",
             "detail": "auto"},
            {"type": "input_text",
             "text": "[audio/wav audio, 8 base64 bytes — omitted: this upstream's tool-result slot is text-only]"},
        ]})
    );
}

#[test]
fn user_images_by_url_and_inline() {
    let b = body(
        json!({"model": "a", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "Compare:"},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
            {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/"}},
        ]}]}),
        false,
    );
    assert_eq!(
        b["input"][0]["content"],
        json!([
            {"type": "input_text", "text": "Compare:"},
            {"type": "input_image", "image_url": "https://example.com/a.png", "detail": "auto"},
            {"type": "input_image", "image_url": "data:image/jpeg;base64,/9j/", "detail": "auto"},
        ])
    );
}

#[test]
fn structured_output_is_text_format() {
    let format = |rf: Value| {
        body(
            json!({"model": "a", "response_format": rf,
                   "messages": [{"role": "user", "content": "x"}]}),
            false,
        )["text"]
            .clone()
    };
    assert_eq!(
        format(json!({"type": "json_schema", "json_schema": {
            "name": "w", "description": "A word", "strict": true,
            "schema": {"type": "object", "properties": {"word": {"type": "string"}}}}})),
        json!({"format": {"type": "json_schema", "name": "w", "description": "A word",
                          "strict": true,
                          "schema": {"type": "object",
                                     "properties": {"word": {"type": "string"}}}}})
    );
    assert_eq!(
        format(json!({"type": "json_object"})),
        json!({"format": {"type": "json_object"}})
    );
    assert_eq!(
        format(json!({"type": "text"})),
        json!({"format": {"type": "text"}})
    );
    // A type lmgw does not model goes as sent: OpenAI answers for it.
    assert_eq!(
        format(json!({"type": "regex", "pattern": "a+"})),
        json!({"format": {"type": "regex", "pattern": "a+"}})
    );
}

#[test]
fn verbosity_goes_into_text_and_other_keys_go_verbatim() {
    let b = body(
        json!({"model": "a", "verbosity": "low", "user": "u1", "service_tier": "flex",
               "store": true, "prompt_cache_key": "k",
               "response_format": {"type": "json_object"},
               "messages": [{"role": "user", "content": "x"}]}),
        false,
    );
    assert_eq!(
        b["text"],
        json!({"format": {"type": "json_object"}, "verbosity": "low"})
    );
    assert!(b.get("verbosity").is_none());
    assert_eq!(b["user"], json!("u1"));
    assert_eq!(b["prompt_cache_key"], json!("k"));
    // The client's own tier and `store` are the client's.
    assert_eq!(b["service_tier"], json!("flex"));
    assert_eq!(b["store"], json!(true));
}

#[test]
fn reasoning_levels_off_and_the_openrouter_object() {
    let reasoning = |extra: Value| {
        let mut v = json!({"model": "a", "messages": [{"role": "user", "content": "x"}]});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let b = body(v, false);
        assert!(b.get("reasoning_effort").is_none(), "{b}");
        b["reasoning"].clone()
    };
    assert_eq!(
        reasoning(json!({"reasoning_effort": "high"})),
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(
        reasoning(json!({"reasoning_effort": "none"})),
        json!({"effort": "none", "summary": "auto"})
    );
    // OpenRouter's object is read into the control and not forwarded.
    assert_eq!(
        reasoning(json!({"reasoning": {"effort": "low", "exclude": true}})),
        json!({"effort": "low", "summary": "auto"})
    );
    assert_eq!(
        reasoning(json!({"reasoning": {"enabled": false}})),
        json!({"effort": "none", "summary": "auto"})
    );
    assert_eq!(reasoning(json!({})), json!({"summary": "auto"}));
}

#[test]
fn the_cap_the_samplers_and_the_stream() {
    let b = body(
        json!({"model": "a", "max_tokens": 64, "temperature": 0.5, "top_p": 0.9,
               "top_k": 20, "min_p": 0.05, "repeat_penalty": 1.1,
               "messages": [{"role": "user", "content": "x"}]}),
        true,
    );
    assert_eq!(b["max_output_tokens"], json!(64));
    assert_eq!(b["temperature"], json!(0.5));
    assert_eq!(b["top_p"], json!(0.9));
    assert_eq!(b["stream"], json!(true));
    for k in [
        "max_tokens",
        "max_completion_tokens",
        "top_k",
        "min_p",
        "repeat_penalty",
        "stream_options",
        "messages",
    ] {
        assert!(b.get(k).is_none(), "{k} in {b}");
    }
}

/// Reasoning parts are not sent without a seal, and a system message with
/// no text is nothing.
#[test]
fn unsealed_reasoning_and_empty_system_text_are_not_sent() {
    let ir: ChatRequest = serde_json::from_value(json!({
        "model_alias": "a",
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": ""}]},
            {"role": "user", "content": [{"type": "text", "text": "x"}]},
            {"role": "assistant", "content": [
                {"type": "reasoning", "text": "thinking"},
                {"type": "text", "text": "y"}]},
        ],
    }))
    .unwrap();
    let b = responses_body(&ir, "gpt-x", &Params::default(), false, &up());
    assert!(b.get("instructions").is_none());
    assert_eq!(b["input"][1], json!({"role": "assistant", "content": "y"}));
}

/// A route that refused a summary is asked for none: an effort stays, and
/// with nothing left there is no `reasoning` at all.
#[test]
fn a_refused_summary_is_left_out() {
    let ir = chat(json!({"model": "a", "reasoning_effort": "low",
                         "messages": [{"role": "user", "content": "x"}]}));
    let mut p = ir.params.clone();
    p.reasoning_summary_refused = true;
    assert_eq!(
        responses_body(&ir, "gpt-x", &p, false, &up())["reasoning"],
        json!({"effort": "low"})
    );
    p.reasoning = None;
    assert!(responses_body(&ir, "gpt-x", &p, false, &up())
        .get("reasoning")
        .is_none());
}

/// What the client said goes as it said it (WP7 review F-R4): a tool's
/// `strict`, a Responses client's `reasoning.summary` (a kind, or `null`
/// for none) and its `text.verbosity`; lmgw's `summary: auto` only where it
/// said nothing. A tool without `strict` keeps its own dialect's default: a
/// chat or Anthropic client's is not strict, a Responses client's is
/// OpenAI's own (no key: strict when its schema allows).
#[test]
fn a_client_s_strict_summary_and_verbosity_go_as_it_sent_them() {
    let tool = |strict: Value| {
        let mut f = json!({"name": "f", "parameters": {"type": "object"}});
        if !strict.is_null() {
            f["strict"] = strict;
        }
        let b = body(
            json!({"model": "a", "messages": [{"role": "user", "content": "x"}],
                   "tools": [{"type": "function", "function": f}]}),
            false,
        );
        b["tools"][0].get("strict").cloned()
    };
    assert_eq!(tool(json!(true)), Some(json!(true)));
    assert_eq!(tool(json!(false)), Some(json!(false)));
    assert_eq!(
        tool(Value::Null),
        Some(json!(false)),
        "none said: Chat Completions' default"
    );

    let responses = |v: Value| {
        let ir = crate::ingress::responses::parse_request(&v).unwrap().ir;
        responses_body(&ir, "gpt-x", &ir.params, false, &up())
    };
    let responses_tool = |strict: Value| {
        let mut t = json!({"type": "function", "name": "f", "parameters": {"type": "object"}});
        if !strict.is_null() {
            t["strict"] = strict;
        }
        // The loop fills the IR's tools from the client's (and its MCP set).
        let req = crate::ingress::responses::parse_request(
            &json!({"model": "a", "input": "x", "tools": [t]}),
        )
        .unwrap();
        let mut ir = req.ir;
        ir.tools = req.client_tools;
        responses_body(&ir, "gpt-x", &ir.params, false, &up())["tools"][0]
            .get("strict")
            .cloned()
    };
    assert_eq!(responses_tool(json!(false)), Some(json!(false)));
    assert_eq!(responses_tool(json!(true)), Some(json!(true)));
    assert_eq!(
        responses_tool(Value::Null),
        None,
        "none said: OpenAI's own default, no key"
    );
    let anthropic = crate::ingress::anthropic::parse_messages_request(&json!({
        "model": "a", "max_tokens": 16,
        "messages": [{"role": "user", "content": "x"}],
        "tools": [{"name": "f", "input_schema": {"type": "object"}},
                  {"name": "g", "input_schema": {"type": "object"}, "strict": true}],
    }))
    .unwrap();
    let b = responses_body(&anthropic, "gpt-x", &anthropic.params, false, &up());
    assert_eq!(b["tools"][0]["strict"], json!(false), "{b}");
    assert_eq!(b["tools"][1]["strict"], json!(true), "{b}");
    let ask = |reasoning: Value| {
        responses(json!({"model": "a", "input": "x", "reasoning": reasoning}))["reasoning"].clone()
    };
    assert_eq!(
        ask(json!({"effort": "low", "summary": "detailed"})),
        json!({"effort": "low", "summary": "detailed"})
    );
    assert_eq!(
        ask(json!({"effort": "low", "summary": null})),
        json!({"effort": "low"}),
        "null: no summary"
    );
    assert_eq!(
        ask(json!({"effort": "low"})),
        json!({"effort": "low", "summary": "auto"})
    );
    let b = responses(json!({"model": "a", "input": "x", "text": {"verbosity": "low"}}));
    assert_eq!(b["text"], json!({"verbosity": "low"}), "{b}");
    assert!(b.get("verbosity").is_none(), "{b}");
    // A client's own kind is never the lesson's to take back.
    let ir = crate::ingress::responses::parse_request(
        &json!({"model": "a", "input": "x", "reasoning": {"summary": "concise"}}),
    )
    .unwrap()
    .ir;
    let mut p = ir.params.clone();
    p.reasoning_summary_refused = true;
    assert_eq!(
        responses_body(&ir, "gpt-x", &p, false, &up())["reasoning"],
        json!({"summary": "concise"})
    );
}

/// The Chat wire (every OpenAI-shaped egress) writes a strict tool's
/// `strict: true`; not strict is its own default, so a `false` and a tool
/// without one both go with no key.
#[test]
fn the_chat_wire_writes_a_tool_s_strict_as_sent() {
    for (strict, want) in [
        (json!(true), json!(true)),
        (json!(false), Value::Null),
        (Value::Null, Value::Null),
    ] {
        let mut f = json!({"name": "f", "parameters": {"type": "object"}});
        if !strict.is_null() {
            f["strict"] = strict;
        }
        let ir = chat(
            json!({"model": "a", "messages": [{"role": "user", "content": "x"}],
                             "tools": [{"type": "function", "function": f}]}),
        );
        let b = crate::egress::openai_compatible::chat_body(&ir, "m", &ir.params, false);
        assert_eq!(b["tools"][0]["function"]["strict"], want, "{b}");
    }
}
