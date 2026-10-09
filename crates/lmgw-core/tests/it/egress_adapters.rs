//! Adapter golden tests (§15): IR → provider request shapes, provider
//! responses → IR, and streamed chunk sequences → IR deltas.

use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::{for_protocol, CountPlan};
use lmgw_core::ir::*;
use lmgw_core::sse::SseEvent;
use serde_json::{json, Value};

fn upstream(protocol: Protocol, base: &str) -> Upstream {
    Upstream {
        id: 1,
        name: "up".into(),
        protocol,
        kind: UpstreamKind::Generic,
        base_url: base.into(),
        api_key: Some("sk-test".into()),
        extra_headers: vec![("x-custom".into(), "yes".into())],
        timeout_ms: 1000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn sample_ir() -> ChatRequest {
    ChatRequest {
        model_alias: "alias".into(),
        messages: vec![
            Message::text(Role::System, "sys prompt"),
            Message::text(Role::User, "hello"),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    args: json!({"city": "Berlin"}),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    id: "call_1".into(),
                    name: None,
                    content: ToolResultBlock::one("12°C"),
                    is_error: false,
                }],
            },
        ],
        params: Params::default(),
        tools: vec![ToolDef {
            name: "get_weather".into(),
            description: Some("weather".into()),
            parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"$schema":"x"}),
        }],
        tool_choice: Some(ToolChoice::Auto),
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

fn body_of(req: &reqwest::Request) -> Value {
    serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
}

fn params() -> Params {
    Params {
        temperature: Some(0.5),
        top_k: Some(40),
        max_tokens: Some(512),
        stop: vec!["END".into()],
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// OpenAI egress
// ---------------------------------------------------------------------------

#[test]
fn openai_build_request() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Openai, "http://h:1/v1/");
    let req = for_protocol(Protocol::Openai)
        .build_chat(&http, &up, "real-model", &sample_ir(), &params(), true)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(req.url().as_str(), "http://h:1/v1/chat/completions");
    assert_eq!(req.headers()["authorization"], "Bearer sk-test");
    assert_eq!(req.headers()["x-custom"], "yes");
    let body = body_of(&req);
    assert_eq!(body["model"], "real-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["temperature"], 0.5);
    assert_eq!(body["top_k"], 40);
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["stop"][0], "END");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
    assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
}

#[test]
fn openai_body_carries_input_audio_verbatim() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Openai, "http://h:1/v1/");
    let mut ir = sample_ir();
    ir.messages.push(Message {
        role: Role::User,
        content: vec![ContentPart::Audio {
            mime: "audio/wav".into(),
            data: "QUFB".into(),
        }],
    });
    let req = for_protocol(Protocol::Openai)
        .build_chat(&http, &up, "m", &ir, &Params::default(), false)
        .unwrap()
        .build()
        .unwrap();
    let body = body_of(&req);
    let last = body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(
        last["content"][0],
        json!({"type": "input_audio", "input_audio": {"data": "QUFB", "format": "wav"}})
    );
}

#[test]
fn openai_emits_passthrough_fields_verbatim() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Openai, "http://h:1/v1/");
    let mut ir = sample_ir();
    ir.passthrough = json!({
        "response_format": {"type": "json_schema",
            "json_schema": {"name": "out", "schema": {"type": "object"}}},
        "min_p": 0.05,
        // A modeled key that somehow lands in passthrough must NOT clobber the
        // egress-built value (params() sets temperature = 0.5).
        "temperature": 0.999,
    })
    .as_object()
    .unwrap()
    .clone();
    let req = for_protocol(Protocol::Openai)
        .build_chat(&http, &up, "real-model", &ir, &params(), false)
        .unwrap()
        .build()
        .unwrap();
    let body = body_of(&req);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["name"], "out");
    assert_eq!(body["min_p"], 0.05);
    // Modeled field wins over the passthrough collision.
    assert_eq!(body["temperature"], 0.5);
}

#[test]
fn openai_parse_completion_and_errors() {
    let egress = for_protocol(Protocol::Openai);
    let resp = json!({
        "model": "gpt-x",
        "choices": [{"message": {"role": "assistant", "content": "hi",
            "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": "{\"a\":1}"}}]},
            "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 9}
    });
    let c = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap();
    assert_eq!(c.model, "gpt-x");
    assert_eq!(c.finish_reason, FinishReason::ToolUse);
    assert_eq!(c.usage.completion_tokens, Some(9));
    assert!(matches!(&c.content[0], ContentPart::Text { text } if text == "hi"));
    assert!(matches!(&c.content[1], ContentPart::ToolUse { args, .. } if args["a"] == 1));

    let err = egress.map_error(
        429,
        br#"{"error": {"message": "rate limited", "type": "rate_limit_error"}}"#,
    );
    let lmgw_core::error::GatewayError::Upstream {
        status,
        provider_type,
        message,
    } = err
    else {
        panic!("wrong error kind");
    };
    assert_eq!(status, 429);
    assert_eq!(provider_type.as_deref(), Some("rate_limit_error"));
    assert_eq!(message, "rate limited");
}

/// llama-server's over-long-prompt refusal (ladder design §2.1 fact 3), the
/// verbatim body measured on this machine's image — `map_error` must turn it
/// into `ContextExceeded`, not the generic `Upstream`, so a client always
/// sees the stable `context_length_exceeded` code.
#[test]
fn llama_exceed_context_size_maps_to_context_exceeded() {
    let egress = for_protocol(Protocol::Openai);
    let body = br#"{"error":{"code":400,"message":"request (8010 tokens) exceeds the available context size (4096 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":8010,"n_ctx":4096}}"#;
    let err = egress.map_error(400, body);
    let lmgw_core::error::GatewayError::ContextExceeded {
        prompt_tokens,
        max_output,
        limit,
        ..
    } = err
    else {
        panic!("wrong error kind: {err:?}");
    };
    assert_eq!(prompt_tokens, 8010);
    assert_eq!(max_output, None);
    assert_eq!(limit, 4096);
}

/// A generic upstream 400 (or any body that is not
/// `exceed_context_size_error`) must keep falling through to the plain
/// `Upstream` mapping — this backstop only fires on the one shape it knows.
#[test]
fn a_different_400_body_is_not_mistaken_for_context_exceeded() {
    let egress = for_protocol(Protocol::Openai);
    let err = egress.map_error(
        400,
        br#"{"error": {"message": "bad request", "type": "invalid_request_error"}}"#,
    );
    assert!(matches!(
        err,
        lmgw_core::error::GatewayError::Upstream { .. }
    ));
}

#[test]
fn parse_exceed_context_reads_the_verbatim_body() {
    use lmgw_core::egress::llama_cpp::parse_exceed_context;
    let body = br#"{"error":{"code":400,"message":"request (8010 tokens) exceeds the available context size (4096 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":8010,"n_ctx":4096}}"#;
    let ec = parse_exceed_context(body).expect("parses the exceed_context_size_error shape");
    assert_eq!(ec.n_prompt_tokens, 8010);
    assert_eq!(ec.n_ctx, 4096);
}

#[test]
fn parse_exceed_context_ignores_the_shared_pool_overflow_shape() {
    use lmgw_core::egress::llama_cpp::parse_exceed_context;
    // Fact 3's *other* body: same family of error, no n_prompt_tokens/n_ctx —
    // must not be misread as the over-long-prompt refusal.
    let body = br#"{"error":{"code":500,"message":"Context size has been exceeded.",
        "type":"server_error"}}"#;
    assert!(parse_exceed_context(body).is_none());
}

/// `chat_body` is what `build_chat` now posts through, so the request the
/// server actually receives must still be exactly what `request_body` used
/// to build — no observable change from the rename.
#[test]
fn chat_body_matches_what_build_chat_posts() {
    use lmgw_core::egress::openai::chat_body;
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Openai, "http://h:1/v1/");
    let ir = sample_ir();
    let p = params();
    let req = for_protocol(Protocol::Openai)
        .build_chat(&http, &up, "real-model", &ir, &p, true)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(body_of(&req), chat_body(&ir, "real-model", &p, true));
}

#[test]
fn openai_stream_decode() {
    let egress = for_protocol(Protocol::Openai);
    let mut dec = egress.new_decoder();
    let chunks = [
        json!({"choices":[{"delta":{"role":"assistant","content":""}}]}),
        json!({"choices":[{"delta":{"content":"Hel"}}]}),
        json!({"choices":[{"delta":{"content":"lo"}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"f","arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":2}"}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":7}}),
    ];
    let mut deltas = Vec::new();
    for c in chunks {
        deltas.extend(dec.on_event(&SseEvent {
            event: None,
            data: c.to_string(),
        }));
    }
    deltas.extend(dec.on_event(&SseEvent {
        event: None,
        data: "[DONE]".into(),
    }));
    assert_eq!(
        deltas,
        vec![
            StreamDelta::TextDelta("Hel".into()),
            StreamDelta::TextDelta("lo".into()),
            StreamDelta::ToolCallStart {
                index: 0,
                id: "c2".into(),
                name: "f".into()
            },
            StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: "{\"x\":2}".into()
            },
            StreamDelta::Stop(FinishReason::ToolUse),
            StreamDelta::Usage(Usage {
                prompt_tokens: Some(5),
                completion_tokens: Some(7),
                ..Default::default()
            }),
        ]
    );
}

// ---------------------------------------------------------------------------
// Anthropic egress
// ---------------------------------------------------------------------------

#[test]
fn anthropic_build_request() {
    let http = reqwest::Client::new();
    // base given WITH /v1 — adapter must not double it
    let up = upstream(Protocol::Anthropic, "https://api.anthropic.com/v1");
    let req = for_protocol(Protocol::Anthropic)
        .build_chat(&http, &up, "claude-x", &sample_ir(), &params(), false)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(req.url().as_str(), "https://api.anthropic.com/v1/messages");
    assert_eq!(req.headers()["x-api-key"], "sk-test");
    assert_eq!(req.headers()["anthropic-version"], "2023-06-01");
    let body = body_of(&req);
    assert_eq!(body["model"], "claude-x");
    assert_eq!(body["system"], "sys prompt");
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["top_k"], 40);
    assert_eq!(body["stop_sequences"][0], "END");
    // messages: user(hello), assistant(tool_use), user(tool_result)
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(body["tool_choice"]["type"], "auto");
}

#[test]
fn anthropic_default_max_tokens_applied() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Anthropic, "https://api.anthropic.com");
    let req = for_protocol(Protocol::Anthropic)
        .build_chat(
            &http,
            &up,
            "claude-x",
            &sample_ir(),
            &Params::default(),
            false,
        )
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(body_of(&req)["max_tokens"], 4096);
}

#[test]
fn anthropic_rejects_audio_input() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Anthropic, "https://api.anthropic.com");
    let mut ir = sample_ir();
    ir.messages.push(Message {
        role: Role::User,
        content: vec![ContentPart::Audio {
            mime: "audio/wav".into(),
            data: "QUFB".into(),
        }],
    });
    let err = for_protocol(Protocol::Anthropic)
        .build_chat(&http, &up, "claude-x", &ir, &Params::default(), false)
        .unwrap_err();
    assert!(matches!(
        err,
        lmgw_core::error::GatewayError::Unsupported(_)
    ));
}

#[test]
fn anthropic_parse_completion() {
    let resp = json!({
        "type": "message",
        "model": "claude-y",
        "content": [
            {"type": "text", "text": "sure"},
            {"type": "tool_use", "id": "tu_1", "name": "f", "input": {"q": 1}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 20, "output_tokens": 11}
    });
    let c = for_protocol(Protocol::Anthropic)
        .parse_completion(resp.to_string().as_bytes())
        .unwrap();
    assert_eq!(c.finish_reason, FinishReason::ToolUse);
    assert_eq!(c.usage.prompt_tokens, Some(20));
    assert!(matches!(&c.content[1], ContentPart::ToolUse { id, .. } if id == "tu_1"));
}

#[test]
fn anthropic_stream_decode() {
    let mut dec = for_protocol(Protocol::Anthropic).new_decoder();
    let events = [
        (
            "message_start",
            json!({"type":"message_start","message":{"usage":{"input_tokens":25,"output_tokens":1}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_9","name":"f","input":{}}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":17}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ];
    let mut deltas = Vec::new();
    for (name, data) in events {
        deltas.extend(dec.on_event(&SseEvent {
            event: Some(name.into()),
            data: data.to_string(),
        }));
    }
    assert_eq!(
        deltas,
        vec![
            StreamDelta::Usage(Usage {
                prompt_tokens: Some(25),
                completion_tokens: Some(1),
                ..Default::default()
            }),
            StreamDelta::TextDelta("Hi".into()),
            StreamDelta::ToolCallStart {
                index: 0,
                id: "tu_9".into(),
                name: "f".into()
            },
            StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: "{\"a\":1}".into()
            },
            StreamDelta::Usage(Usage {
                prompt_tokens: None,
                completion_tokens: Some(17),
                ..Default::default()
            }),
            StreamDelta::Stop(FinishReason::ToolUse),
        ]
    );
}

// ---------------------------------------------------------------------------
// Gemini egress
// ---------------------------------------------------------------------------

#[test]
fn gemini_build_request() {
    let http = reqwest::Client::new();
    let up = upstream(
        Protocol::Gemini,
        "https://generativelanguage.googleapis.com",
    );
    let req = for_protocol(Protocol::Gemini)
        .build_chat(&http, &up, "gemini-pro", &sample_ir(), &params(), true)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        req.url().as_str(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-pro:streamGenerateContent?alt=sse"
    );
    assert_eq!(req.headers()["x-goog-api-key"], "sk-test");
    let body = body_of(&req);
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys prompt");
    let contents = body["contents"].as_array().unwrap();
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(
        contents[1]["parts"][0]["functionCall"]["name"],
        "get_weather"
    );
    // tool result correlated by NAME (resolved from the call id)
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["name"],
        "get_weather"
    );
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["response"]["result"],
        "12°C"
    );
    assert_eq!(body["generationConfig"]["temperature"], 0.5);
    assert_eq!(body["generationConfig"]["topK"], 40);
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 512);
    let decl = &body["tools"][0]["functionDeclarations"][0];
    assert_eq!(decl["name"], "get_weather");
    // $schema must be sanitized away
    assert!(decl["parameters"].get("$schema").is_none());
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
}

#[test]
fn gemini_parse_completion() {
    let resp = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"text": "answer"},
                {"functionCall": {"name": "f", "args": {"k": "v"}}}
            ]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 4},
        "modelVersion": "gemini-pro-001"
    });
    let c = for_protocol(Protocol::Gemini)
        .parse_completion(resp.to_string().as_bytes())
        .unwrap();
    // STOP + functionCall present → ToolUse finish reason
    assert_eq!(c.finish_reason, FinishReason::ToolUse);
    assert_eq!(c.usage.prompt_tokens, Some(8));
    assert!(
        matches!(&c.content[1], ContentPart::ToolUse { id, name, .. }
        if id.starts_with("call_") && id.len() > 17 && name == "f")
    );
}

#[test]
fn gemini_stream_decode() {
    let mut dec = for_protocol(Protocol::Gemini).new_decoder();
    let chunks = [
        json!({"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"text":"lo"}]}}],
               "usageMetadata":{"promptTokenCount":6,"candidatesTokenCount":1}}),
        json!({"candidates":[{"content":{"parts":[{"text":"!"}]},"finishReason":"STOP"}],
               "usageMetadata":{"promptTokenCount":6,"candidatesTokenCount":3}}),
    ];
    let mut deltas = Vec::new();
    for c in chunks {
        deltas.extend(dec.on_event(&SseEvent {
            event: None,
            data: c.to_string(),
        }));
    }
    assert_eq!(
        deltas,
        vec![
            StreamDelta::TextDelta("Hel".into()),
            StreamDelta::TextDelta("lo".into()),
            StreamDelta::TextDelta("!".into()),
            StreamDelta::Usage(Usage {
                prompt_tokens: Some(6),
                completion_tokens: Some(3),
                ..Default::default()
            }),
            StreamDelta::Stop(FinishReason::Stop),
        ]
    );
}

#[test]
fn gemini_body_carries_audio_as_inline_data() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Gemini, "https://g");
    let mut ir = sample_ir();
    ir.messages.push(Message {
        role: Role::User,
        content: vec![ContentPart::Audio {
            mime: "audio/wav".into(),
            data: "QUFB".into(),
        }],
    });
    let req = for_protocol(Protocol::Gemini)
        .build_chat(&http, &up, "m", &ir, &Params::default(), false)
        .unwrap()
        .build()
        .unwrap();
    let body = body_of(&req);
    let contents = body["contents"].as_array().unwrap();
    let parts = contents.last().unwrap()["parts"].as_array().unwrap();
    assert_eq!(
        parts.last().unwrap(),
        &json!({"inlineData": {"mimeType": "audio/wav", "data": "QUFB"}})
    );
}

#[test]
fn gemini_rejects_url_images() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Gemini, "https://g");
    let mut ir = sample_ir();
    ir.messages.push(Message {
        role: Role::User,
        content: vec![ContentPart::Image {
            mime: "image/png".into(),
            source: ImageSource::Url {
                url: "https://x/y.png".into(),
            },
        }],
    });
    let err = for_protocol(Protocol::Gemini)
        .build_chat(&http, &up, "m", &ir, &Params::default(), false)
        .unwrap_err();
    assert!(matches!(
        err,
        lmgw_core::error::GatewayError::Unsupported(_)
    ));
}

// ---------------------------------------------------------------------------
// Token counting (count_tokens)
// ---------------------------------------------------------------------------

fn count_request(plan: CountPlan) -> reqwest::Request {
    match plan {
        CountPlan::Request(rb) => rb.build().unwrap(),
        CountPlan::Ready(_) | CountPlan::Guessed(_) => {
            panic!("expected a remote count request, got a local count")
        }
    }
}

#[test]
fn openai_llama_count_tokens_is_native_tokenize() {
    let http = reqwest::Client::new();
    let mut up = upstream(Protocol::LlamaCpp, "http://h:1/v1");
    up.kind = UpstreamKind::LlamaServer;
    let plan = for_protocol(up.protocol)
        .build_count_tokens(&http, &up, "local-model", "hello world")
        .unwrap();
    let req = count_request(plan);
    // /tokenize lives at the server root, not under /v1
    assert_eq!(req.url().as_str(), "http://h:1/tokenize");
    assert_eq!(req.headers()["authorization"], "Bearer sk-test");
    let body = body_of(&req);
    assert_eq!(body["content"], "hello world");
    // model must be forwarded so router-mode llama-server can pick a backend
    assert_eq!(body["model"], "local-model");

    // response: count = number of returned token ids
    let n = for_protocol(up.protocol)
        .parse_count(br#"{"tokens":[1,2,3,4]}"#)
        .unwrap();
    assert_eq!(n, 4);
}

#[test]
fn openai_generic_count_tokens_uses_tiktoken_locally() {
    let http = reqwest::Client::new();
    // kind defaults to Generic → real-OpenAI path (no /tokenize endpoint).
    let up = upstream(Protocol::Openai, "https://api.openai.com/v1");
    let plan = for_protocol(Protocol::Openai)
        .build_count_tokens(&http, &up, "gpt-4o", "hello world")
        .unwrap();
    match plan {
        CountPlan::Ready(n) => assert!(n >= 2, "expected a local tiktoken count, got {n}"),
        CountPlan::Guessed(_) => panic!("gpt-4o's encoding is known; nothing was guessed"),
        CountPlan::Request(_) => panic!("real OpenAI has no token endpoint; expected local count"),
    }
    // An unknown / future model name still counts (o200k_base fallback), and
    // says the tokenizer was a guess (api-docs design §5.1).
    let plan = for_protocol(Protocol::Openai)
        .build_count_tokens(&http, &up, "gpt-9-something", "hello world")
        .unwrap();
    assert!(matches!(plan, CountPlan::Guessed(n) if n >= 2));
}

#[test]
fn anthropic_count_tokens_is_native_count_endpoint() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Anthropic, "https://api.anthropic.com/v1");
    let plan = for_protocol(Protocol::Anthropic)
        .build_count_tokens(&http, &up, "claude-x", "hello world")
        .unwrap();
    let req = count_request(plan);
    assert_eq!(
        req.url().as_str(),
        "https://api.anthropic.com/v1/messages/count_tokens"
    );
    assert_eq!(req.headers()["x-api-key"], "sk-test");
    assert_eq!(req.headers()["anthropic-version"], "2023-06-01");
    let body = body_of(&req);
    assert_eq!(body["model"], "claude-x");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "hello world");

    let n = for_protocol(Protocol::Anthropic)
        .parse_count(br#"{"input_tokens":42}"#)
        .unwrap();
    assert_eq!(n, 42);
}

#[test]
fn gemini_count_tokens_is_native_count_endpoint() {
    let http = reqwest::Client::new();
    let up = upstream(
        Protocol::Gemini,
        "https://generativelanguage.googleapis.com",
    );
    let plan = for_protocol(Protocol::Gemini)
        .build_count_tokens(&http, &up, "gemini-pro", "hello world")
        .unwrap();
    let req = count_request(plan);
    assert_eq!(
        req.url().as_str(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-pro:countTokens"
    );
    assert_eq!(req.headers()["x-goog-api-key"], "sk-test");
    assert_eq!(
        body_of(&req)["contents"][0]["parts"][0]["text"],
        "hello world"
    );

    let n = for_protocol(Protocol::Gemini)
        .parse_count(br#"{"totalTokens":7}"#)
        .unwrap();
    assert_eq!(n, 7);
}

// ---------------------------------------------------------------------------
// Cross-protocol translation: OpenAI request in → IR → each egress
// ---------------------------------------------------------------------------

#[test]
fn openai_in_anthropic_out_roundtrip() {
    let body = json!({
        "model": "alias",
        "messages": [
            {"role": "system", "content": "S"},
            {"role": "user", "content": "U"}
        ],
        "max_tokens": 99
    });
    let ir = lmgw_core::ingress::openai::parse_chat_request(&body).unwrap();
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Anthropic, "https://api.anthropic.com");
    let req = for_protocol(Protocol::Anthropic)
        .build_chat(&http, &up, "claude", &ir, &ir.params, false)
        .unwrap()
        .build()
        .unwrap();
    let b = body_of(&req);
    assert_eq!(b["system"], "S");
    assert_eq!(b["max_tokens"], 99);
    assert_eq!(b["messages"].as_array().unwrap().len(), 1);
    assert_eq!(b["messages"][0]["content"][0]["text"], "U");
}

#[test]
fn anthropic_in_openai_out_roundtrip() {
    let body = json!({
        "model": "alias",
        "max_tokens": 50,
        "system": "S",
        "messages": [{"role": "user", "content": "U"}]
    });
    let ir = lmgw_core::ingress::anthropic::parse_messages_request(&body).unwrap();
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Openai, "http://h/v1");
    let req = for_protocol(Protocol::Openai)
        .build_chat(&http, &up, "gpt", &ir, &ir.params, false)
        .unwrap()
        .build()
        .unwrap();
    let b = body_of(&req);
    assert_eq!(b["messages"][0]["role"], "system");
    assert_eq!(b["messages"][0]["content"], "S");
    assert_eq!(b["messages"][1]["content"], "U");
    assert_eq!(b["max_tokens"], 50);
}

// ---------------------------------------------------------------------------
// Reasoning ("thinking") is surfaced, not dropped or merged into the answer.
// ---------------------------------------------------------------------------

#[test]
fn openai_reasoning_content_decoded_and_parsed() {
    let egress = for_protocol(Protocol::Openai);
    let mut dec = egress.new_decoder();
    let chunks = [
        json!({"choices":[{"delta":{"role":"assistant","reasoning_content":"Th","content":null}}]}),
        json!({"choices":[{"delta":{"reasoning_content":"ink"}}]}),
        json!({"choices":[{"delta":{"content":"42"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    ];
    let mut deltas = Vec::new();
    for c in chunks {
        deltas.extend(dec.on_event(&SseEvent {
            event: None,
            data: c.to_string(),
        }));
    }
    assert_eq!(
        deltas,
        vec![
            StreamDelta::ReasoningDelta("Th".into()),
            StreamDelta::ReasoningDelta("ink".into()),
            StreamDelta::TextDelta("42".into()),
            StreamDelta::Stop(FinishReason::Stop),
        ]
    );
    let resp = json!({"model":"m","choices":[{"message":
        {"role":"assistant","content":"42","reasoning_content":"Think"},"finish_reason":"stop"}]});
    let c = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap();
    assert_eq!(c.reasoning, "Think");
    assert!(matches!(&c.content[0], ContentPart::Text { text } if text == "42"));
}

#[test]
fn anthropic_thinking_decoded_and_parsed() {
    let egress = for_protocol(Protocol::Anthropic);
    let mut dec = egress.new_decoder();
    let events = [
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pon"}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"der"}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"xyz"}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi"}}),
        ),
    ];
    let mut deltas = Vec::new();
    for (e, v) in events {
        deltas.extend(dec.on_event(&SseEvent {
            event: Some(e.into()),
            data: v.to_string(),
        }));
    }
    assert_eq!(
        deltas,
        vec![
            StreamDelta::ReasoningDelta("pon".into()),
            StreamDelta::ReasoningDelta("der".into()),
            StreamDelta::TextDelta("hi".into()),
        ]
    );
    let resp = json!({"model":"claude","content":[
        {"type":"thinking","thinking":"pondering"},{"type":"text","text":"hi"}],
        "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}});
    let c = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap();
    assert_eq!(c.reasoning, "pondering");
    assert!(matches!(&c.content[0], ContentPart::Text { text } if text == "hi"));
}

#[test]
fn gemini_thought_split_from_answer() {
    let egress = for_protocol(Protocol::Gemini);
    let mut dec = egress.new_decoder();
    let chunks = [
        json!({"candidates":[{"content":{"parts":[{"text":"reasoning here","thought":true}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4}}),
    ];
    let mut deltas = Vec::new();
    for c in chunks {
        deltas.extend(dec.on_event(&SseEvent {
            event: None,
            data: c.to_string(),
        }));
    }
    assert!(deltas.contains(&StreamDelta::ReasoningDelta("reasoning here".into())));
    assert!(deltas.contains(&StreamDelta::TextDelta("answer".into())));
    // Reasoning must NOT leak into the answer text.
    assert!(!deltas.contains(&StreamDelta::TextDelta("reasoning here".into())));

    let resp = json!({"modelVersion":"gemini","candidates":[{"content":{"parts":[
        {"text":"thinking...","thought":true},{"text":"final"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}});
    let c = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap();
    assert_eq!(c.reasoning, "thinking...");
    assert_eq!(c.content.len(), 1);
    assert!(matches!(&c.content[0], ContentPart::Text { text } if text == "final"));
}

// ---------------------------------------------------------------------------
// Tool-result blocks (§7): each adapter emits what its protocol natively takes
// ---------------------------------------------------------------------------

/// An IR carrying one tool result made of `blocks`, and nothing else.
fn ir_with_tool_result(blocks: Vec<ToolResultBlock>) -> ChatRequest {
    ChatRequest {
        model_alias: "alias".into(),
        messages: vec![
            Message::text(Role::User, "go"),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: "call_1".into(),
                    name: "shoot".into(),
                    args: json!({}),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    id: "call_1".into(),
                    name: None,
                    content: blocks,
                    is_error: false,
                }],
            },
        ],
        params: Params::default(),
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

fn built(proto: Protocol, ir: &ChatRequest) -> Value {
    let http = reqwest::Client::new();
    let up = upstream(proto, "http://h:1/v1/");
    let req = for_protocol(proto)
        .build_chat(&http, &up, "m", ir, &Params::default(), false)
        .unwrap()
        .build()
        .unwrap();
    body_of(&req)
}

/// The property the migration promised: a lone text block is indistinguishable
/// from the old `content: String`, on every adapter. This is the whole
/// back-compat guarantee of moving the IR to blocks, so it gets its own test.
#[test]
fn a_lone_text_block_renders_exactly_as_a_bare_string_did() {
    let ir = ir_with_tool_result(ToolResultBlock::one("12°C"));

    let oa = built(Protocol::Openai, &ir);
    assert_eq!(oa["messages"][2]["role"], "tool");
    assert_eq!(oa["messages"][2]["content"], "12°C");

    let an = built(Protocol::Anthropic, &ir);
    let block = &an["messages"][2]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["content"][0], json!({"type": "text", "text": "12°C"}));

    let gm = built(Protocol::Gemini, &ir);
    let resp = &gm["contents"][2]["parts"][0]["functionResponse"]["response"];
    assert_eq!(resp, &json!({"result": "12°C"}));
}

#[test]
fn anthropic_passes_an_image_tool_result_through_natively() {
    let ir = ir_with_tool_result(vec![
        ToolResultBlock::text("here is the screenshot"),
        ToolResultBlock::Image {
            mime: "image/png".into(),
            data: "aGVsbG8=".into(),
        },
    ]);
    let body = built(Protocol::Anthropic, &ir);
    let content = &body["messages"][2]["content"][0]["content"];
    assert_eq!(
        content[0],
        json!({"type": "text", "text": "here is the screenshot"})
    );
    assert_eq!(
        content[1],
        json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="},
        })
    );
}

/// OpenAI's `role: "tool"` message is text-only, so the image cannot survive.
/// What matters is that it degrades to a *named placeholder* — never a silent
/// drop, and never the base64 payload inlined into the prompt (§14).
#[test]
fn openai_replaces_a_binary_tool_result_with_a_named_placeholder() {
    let payload = "A".repeat(4096);
    let ir = ir_with_tool_result(vec![
        ToolResultBlock::text("here is the screenshot"),
        ToolResultBlock::Image {
            mime: "image/png".into(),
            data: payload.clone(),
        },
    ]);
    let body = built(Protocol::Openai, &ir);
    let text = body["messages"][2]["content"].as_str().unwrap();
    assert!(text.starts_with("here is the screenshot\n"));
    assert!(text.contains("image/png image, 4096 base64 bytes"));
    assert!(text.contains("omitted"));
    assert!(
        !text.contains(&payload),
        "the base64 payload must never be inlined into the prompt"
    );
}

#[test]
fn gemini_takes_structured_output_verbatim() {
    let ir = ir_with_tool_result(vec![ToolResultBlock::Json {
        value: json!({"temp_c": 12, "wind": {"kph": 9}}),
    }]);
    let body = built(Protocol::Gemini, &ir);
    let resp = &body["contents"][2]["parts"][0]["functionResponse"]["response"];
    assert_eq!(resp, &json!({"temp_c": 12, "wind": {"kph": 9}}));
}

/// Regression: the old string IR filtered on `is_object`, so a tool returning a
/// JSON *array* reached Gemini as `{"result": "[1,2,3]"}` — the array flattened
/// back into a string. It now arrives as a real array under `result`.
#[test]
fn gemini_no_longer_stringifies_a_json_array() {
    let ir = ir_with_tool_result(vec![ToolResultBlock::Json {
        value: json!([1, 2, 3]),
    }]);
    let body = built(Protocol::Gemini, &ir);
    let resp = &body["contents"][2]["parts"][0]["functionResponse"]["response"];
    assert_eq!(resp, &json!({"result": [1, 2, 3]}));
}

/// Back-compat: a client replaying a tool result whose JSON is carried *as a
/// string* (all the old IR could express) must still reach Gemini structured.
#[test]
fn gemini_still_parses_json_carried_as_text() {
    let ir = ir_with_tool_result(ToolResultBlock::one(r#"{"temp_c":12}"#));
    let body = built(Protocol::Gemini, &ir);
    let resp = &body["contents"][2]["parts"][0]["functionResponse"]["response"];
    assert_eq!(resp, &json!({"temp_c": 12}));
}

/// Anthropic ingress is the only client protocol that can *send* a non-text
/// tool result, so the block must survive parse → IR → Anthropic egress.
#[test]
fn an_image_tool_result_round_trips_from_anthropic_ingress() {
    let body = json!({
        "model": "alias",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "shoot", "input": {}}
            ]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": [
                    {"type": "text", "text": "done"},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/webp", "data": "d2VicA=="
                    }}
                ],
            }]}
        ],
    });
    let ir = lmgw_core::ingress::anthropic::parse_messages_request(&body).unwrap();
    let tool_msg = ir.messages.iter().find(|m| m.role == Role::Tool).unwrap();
    let ContentPart::ToolResult { content, .. } = &tool_msg.content[0] else {
        panic!("expected a tool result");
    };
    assert_eq!(content.len(), 2);
    assert_eq!(
        content[1],
        ToolResultBlock::Image {
            mime: "image/webp".into(),
            data: "d2VicA==".into(),
        }
    );

    let out = built(Protocol::Anthropic, &ir);
    let blocks = &out["messages"][2]["content"][0]["content"];
    assert_eq!(blocks[1]["source"]["media_type"], "image/webp");
}

// ---------------------------------------------------------------------------
// Reasoning replay: the trace reaches the upstream that can use it
// ---------------------------------------------------------------------------

fn ir_with_reasoning(signature: Option<&str>) -> ChatRequest {
    ChatRequest {
        model_alias: "alias".into(),
        messages: vec![
            Message::text(Role::User, "pick a colour"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Reasoning {
                        text: "teal is unusual".into(),
                        signature: signature.map(String::from),
                    },
                    ContentPart::text("Teal"),
                ],
            },
            Message::text(Role::User, "why?"),
        ],
        params: Params::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

#[test]
fn openai_replays_reasoning_as_reasoning_content() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Openai, "http://h/v1");
    let req = for_protocol(Protocol::Openai)
        .build_chat(
            &http,
            &up,
            "m",
            &ir_with_reasoning(None),
            &Params::default(),
            false,
        )
        .unwrap()
        .build()
        .unwrap();
    let b = body_of(&req);
    let asst = &b["messages"][1];
    assert_eq!(asst["role"], "assistant");
    assert_eq!(asst["content"], "Teal");
    assert_eq!(asst["reasoning_content"], "teal is unusual");
    // A turn without a trace does not grow the field.
    assert!(b["messages"][0].get("reasoning_content").is_none());

    // A tool-call turn — the case a thinking model in a tool loop hits on
    // every step — keeps `content: null` and still carries its trace.
    let mut ir = ir_with_reasoning(None);
    ir.messages[1].content[1] = ContentPart::ToolUse {
        id: "c1".into(),
        name: "search".into(),
        args: json!({"q": "teal"}),
    };
    let req = for_protocol(Protocol::Openai)
        .build_chat(&http, &up, "m", &ir, &Params::default(), false)
        .unwrap()
        .build()
        .unwrap();
    let asst = &body_of(&req)["messages"][1];
    assert_eq!(asst["content"], Value::Null);
    assert_eq!(asst["tool_calls"][0]["function"]["name"], "search");
    assert_eq!(asst["reasoning_content"], "teal is unusual");
}

#[test]
fn anthropic_forwards_only_signed_thinking_blocks() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Anthropic, "https://api.anthropic.com");
    let build = |ir: &ChatRequest| {
        let req = for_protocol(Protocol::Anthropic)
            .build_chat(&http, &up, "claude", ir, &Params::default(), false)
            .unwrap()
            .build()
            .unwrap();
        body_of(&req)
    };
    // Signed by Anthropic: goes back verbatim, still leading the turn.
    let b = build(&ir_with_reasoning(Some("sig-1")));
    assert_eq!(
        b["messages"][1]["content"][0],
        json!({"type": "thinking", "thinking": "teal is unusual", "signature": "sig-1"})
    );
    assert_eq!(b["messages"][1]["content"][1]["text"], "Teal");
    // Unsigned (a llama.cpp trace): the API would reject it, so it is dropped.
    let b = build(&ir_with_reasoning(None));
    assert_eq!(
        b["messages"][1]["content"],
        json!([{"type": "text", "text": "Teal"}])
    );
}

#[test]
fn gemini_drops_reasoning_it_cannot_replay() {
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Gemini, "https://g/v1beta");
    let req = for_protocol(Protocol::Gemini)
        .build_chat(
            &http,
            &up,
            "m",
            &ir_with_reasoning(None),
            &Params::default(),
            false,
        )
        .unwrap()
        .build()
        .unwrap();
    let b = body_of(&req);
    assert_eq!(b["contents"][1]["role"], "model");
    assert_eq!(b["contents"][1]["parts"], json!([{"text": "Teal"}]));
}

#[test]
fn anthropic_in_openai_out_replays_thinking_as_reasoning_content() {
    // What an Anthropic-SDK client sends back after this gateway answered it
    // from llama.cpp: the unsigned thinking block `serialize_completion` emits.
    let body = json!({
        "model": "alias",
        "max_tokens": 50,
        "messages": [
            {"role": "user", "content": "pick a colour"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "teal is unusual"},
                {"type": "text", "text": "Teal"}
            ]},
            {"role": "user", "content": "why?"}
        ]
    });
    let ir = lmgw_core::ingress::anthropic::parse_messages_request(&body).unwrap();
    let http = reqwest::Client::new();
    let up = upstream(Protocol::Openai, "http://h/v1");
    let req = for_protocol(Protocol::Openai)
        .build_chat(&http, &up, "gpt", &ir, &ir.params, false)
        .unwrap()
        .build()
        .unwrap();
    let b = body_of(&req);
    assert_eq!(b["messages"][1]["content"], "Teal");
    assert_eq!(b["messages"][1]["reasoning_content"], "teal is unusual");
}

// ---------------------------------------------------------------------------
// Reasoning control → the four routes (model-capabilities design §5.3)
// ---------------------------------------------------------------------------

fn kinded(protocol: Protocol, kind: UpstreamKind) -> Upstream {
    Upstream {
        kind,
        ..upstream(protocol, "http://h:1/v1")
    }
}

/// `params()` plus a reasoning control, so every cell is exercised against the
/// same otherwise-unremarkable request.
fn reasoning_params(c: ReasoningControl) -> Params {
    Params {
        reasoning: Some(c),
        ..params()
    }
}

fn enabled(v: bool) -> ReasoningControl {
    ReasoningControl {
        enabled: Some(v),
        ..Default::default()
    }
}

fn effort(level: &str) -> ReasoningControl {
    ReasoningControl {
        effort: Some(level.into()),
        ..Default::default()
    }
}

fn budget(n: i64) -> ReasoningControl {
    ReasoningControl {
        budget_tokens: Some(n),
        ..Default::default()
    }
}

fn reasoning_body(up: &Upstream, ir: &ChatRequest, params: &Params) -> Value {
    let http = reqwest::Client::new();
    let req = for_protocol(up.protocol)
        .build_chat(&http, up, "real-model", ir, params, false)
        .unwrap()
        .build()
        .unwrap();
    body_of(&req)
}

fn llama_body(c: ReasoningControl) -> Value {
    reasoning_body(
        &kinded(Protocol::LlamaCpp, UpstreamKind::LlamaServer),
        &sample_ir(),
        &reasoning_params(c),
    )
}

fn openai_body(c: ReasoningControl) -> Value {
    reasoning_body(
        &kinded(Protocol::Openai, UpstreamKind::Generic),
        &sample_ir(),
        &reasoning_params(c),
    )
}

fn anthropic_body(c: ReasoningControl) -> Value {
    reasoning_body(
        &kinded(Protocol::Anthropic, UpstreamKind::Generic),
        &sample_ir(),
        &reasoning_params(c),
    )
}

fn gemini_body(c: ReasoningControl) -> Value {
    reasoning_body(
        &kinded(Protocol::Gemini, UpstreamKind::Generic),
        &sample_ir(),
        &reasoning_params(c),
    )
}

/// Off, on the build lmgw actually runs: the template kwarg, not a level.
/// `reasoning_effort: "none"` is a newer-master special case there, and
/// Qwen3.8's template raises on a level it does not know.
#[test]
fn llama_server_disables_with_enable_thinking_false() {
    let b = llama_body(enabled(false));
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
    assert!(b.get("reasoning_effort").is_none());
    assert!(b.get("reasoning_budget_tokens").is_none());
}

#[test]
fn llama_server_enabled_alone_sets_enable_thinking() {
    let b = llama_body(enabled(true));
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], true);
    assert!(b.get("reasoning_effort").is_none());
}

/// A level implies thinking is on, so a `--reasoning off` row honours it.
#[test]
fn llama_server_effort_sets_level_and_enable_thinking() {
    let b = llama_body(effort("xhigh"));
    assert_eq!(b["reasoning_effort"], "xhigh");
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], true);
}

#[test]
fn llama_server_budget_is_forwarded() {
    let b = llama_body(budget(2048));
    assert_eq!(b["reasoning_budget_tokens"], 2048);
}

/// `chat_template_kwargs` is an object-level deep merge: the client's own
/// kwargs survive the control being written into the same object.
#[test]
fn llama_server_deep_merges_chat_template_kwargs() {
    let mut ir = sample_ir();
    ir.passthrough =
        json!({"chat_template_kwargs": {"custom": "keep me", "enable_thinking": false}})
            .as_object()
            .unwrap()
            .clone();
    let b = reasoning_body(
        &kinded(Protocol::LlamaCpp, UpstreamKind::LlamaServer),
        &ir,
        &reasoning_params(effort("high")),
    );
    assert_eq!(b["chat_template_kwargs"]["custom"], "keep me");
    // The resolved control wins over the client's stale value in the same
    // object — the two cannot be allowed to disagree.
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(b["reasoning_effort"], "high");
}

/// A level that rode in through passthrough must not reach a template that has
/// been told not to think.
#[test]
fn llama_server_strips_a_passthrough_level_when_off() {
    let mut ir = sample_ir();
    ir.passthrough = json!({"reasoning_effort": "high"})
        .as_object()
        .unwrap()
        .clone();
    let b = reasoning_body(
        &kinded(Protocol::LlamaCpp, UpstreamKind::LlamaServer),
        &ir,
        &reasoning_params(enabled(false)),
    );
    assert!(b.get("reasoning_effort").is_none());
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
}

#[test]
fn generic_openai_disables_with_effort_none() {
    let b = openai_body(enabled(false));
    assert_eq!(b["reasoning_effort"], "none");
    // No template kwargs on a provider that has no template.
    assert!(b.get("chat_template_kwargs").is_none());
}

#[test]
fn generic_openai_sends_the_level_only() {
    let b = openai_body(effort("high"));
    assert_eq!(b["reasoning_effort"], "high");
    assert!(b.get("chat_template_kwargs").is_none());
}

/// The two cells this route cannot express. They are reported on the response
/// as `x-lmgw-reasoning-ignored` (see `reasoning_ignored` below), never guessed.
#[test]
fn generic_openai_ignores_bare_enabled_and_budget() {
    let b = openai_body(enabled(true));
    assert!(b.get("reasoning_effort").is_none());
    assert!(b.get("chat_template_kwargs").is_none());

    let b = openai_body(budget(4096));
    assert!(b.get("reasoning_budget_tokens").is_none());
    assert!(b.get("reasoning_effort").is_none());
}

/// An OpenRouter-shaped `reasoning` object riding through passthrough gets its
/// control keys rewritten to what lmgw resolved; its own keys are untouched.
#[test]
fn openai_rewrites_a_passthrough_reasoning_object() {
    let mut ir = sample_ir();
    ir.passthrough = json!({"reasoning": {"effort": "low", "exclude": true, "max_tokens": 900}})
        .as_object()
        .unwrap()
        .clone();
    let b = reasoning_body(
        &kinded(Protocol::Openai, UpstreamKind::Generic),
        &ir,
        &reasoning_params(effort("high")),
    );
    assert_eq!(b["reasoning"]["effort"], "high");
    assert_eq!(b["reasoning"]["exclude"], true);
    assert_eq!(b["reasoning"]["max_tokens"], 900);
    // …and only the object: a client that spoke OpenRouter's dialect does not
    // also get a scalar it never sent, which a provider reading both might
    // resolve differently than lmgw would.
    assert!(
        b.get("reasoning_effort").is_none(),
        "no invented scalar beside the client's object: {b}"
    );

    // Switched off, the object says so too rather than carrying a stale level.
    let b = reasoning_body(
        &kinded(Protocol::Openai, UpstreamKind::Generic),
        &ir,
        &reasoning_params(enabled(false)),
    );
    assert_eq!(b["reasoning"]["enabled"], false);
    assert!(b["reasoning"].get("effort").is_none());
    assert_eq!(b["reasoning"]["exclude"], true);
    assert!(b.get("reasoning_effort").is_none());
}

/// The llama-server route is the exception: it does not understand the
/// OpenRouter object at all, so the scalar is the only thing that can carry the
/// control there and is emitted alongside.
#[test]
fn a_llama_route_still_gets_the_scalar_beside_an_openrouter_object() {
    let mut ir = sample_ir();
    ir.passthrough = json!({"reasoning": {"effort": "low"}})
        .as_object()
        .unwrap()
        .clone();
    let b = reasoning_body(
        &kinded(Protocol::LlamaCpp, UpstreamKind::LlamaServer),
        &ir,
        &reasoning_params(effort("high")),
    );
    assert_eq!(b["reasoning_effort"], "high");
    assert_eq!(b["reasoning"]["effort"], "high");
}

/// A plain `reasoning_effort` on a generic upstream is byte-for-byte what it
/// was before the control plane existed: one scalar, nothing else added.
#[test]
fn a_plain_effort_on_a_generic_upstream_adds_exactly_one_key() {
    let up = kinded(Protocol::Openai, UpstreamKind::Generic);
    let before = reasoning_body(&up, &sample_ir(), &params());
    let after = reasoning_body(&up, &sample_ir(), &reasoning_params(effort("high")));

    let mut expected = before.as_object().unwrap().clone();
    expected.insert("reasoning_effort".into(), json!("high"));
    assert_eq!(after, Value::Object(expected));
}

#[test]
fn anthropic_disables_thinking() {
    let b = anthropic_body(enabled(false));
    assert_eq!(b["thinking"]["type"], "disabled");
    assert!(b.get("output_config").is_none());
}

#[test]
fn anthropic_enabled_alone_is_adaptive() {
    let b = anthropic_body(enabled(true));
    assert_eq!(b["thinking"]["type"], "adaptive");
}

#[test]
fn anthropic_effort_is_adaptive_plus_output_config() {
    let b = anthropic_body(effort("high"));
    assert_eq!(b["thinking"]["type"], "adaptive");
    assert_eq!(b["output_config"]["effort"], "high");
}

/// The API refuses `max_tokens <= thinking.budget_tokens`, so the cap is
/// raised rather than the client earning a 400 it did not ask for.
#[test]
fn anthropic_budget_raises_max_tokens_when_smaller() {
    // params() sets max_tokens = 512.
    let b = anthropic_body(budget(8000));
    assert_eq!(b["thinking"]["type"], "enabled");
    assert_eq!(b["thinking"]["budget_tokens"], 8000);
    assert_eq!(b["max_tokens"], 9024);

    // A cap that already clears the budget is left exactly as the client set it.
    let p = Params {
        max_tokens: Some(64_000),
        ..reasoning_params(budget(8000))
    };
    let b = reasoning_body(
        &kinded(Protocol::Anthropic, UpstreamKind::Generic),
        &sample_ir(),
        &p,
    );
    assert_eq!(b["max_tokens"], 64_000);
}

#[test]
fn gemini_disables_with_a_zero_budget() {
    let b = gemini_body(enabled(false));
    assert_eq!(b["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0);
}

#[test]
fn gemini_maps_effort_and_budget() {
    let b = gemini_body(effort("high"));
    assert_eq!(
        b["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    let b = gemini_body(budget(4096));
    assert_eq!(
        b["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        4096
    );
    // A bare `enabled` has no Gemini spelling; nothing is guessed.
    let b = gemini_body(enabled(true));
    assert!(b["generationConfig"].get("thinkingConfig").is_none());
}

/// No control ⇒ no reasoning keys anywhere: a request that never mentions
/// reasoning produces exactly the body it produced before this feature.
#[test]
fn an_empty_control_changes_no_body() {
    for (proto, kind) in [
        (Protocol::LlamaCpp, UpstreamKind::LlamaServer),
        (Protocol::Openai, UpstreamKind::Generic),
        (Protocol::Anthropic, UpstreamKind::Generic),
        (Protocol::Gemini, UpstreamKind::Generic),
    ] {
        let up = kinded(proto, kind);
        let plain = reasoning_body(&up, &sample_ir(), &params());
        let empty = reasoning_body(
            &up,
            &sample_ir(),
            &reasoning_params(ReasoningControl::default()),
        );
        assert_eq!(plain, empty, "{proto:?}/{kind:?}");
        assert!(
            !serde_json::to_string(&plain).unwrap().contains("thinking"),
            "{proto:?}/{kind:?}"
        );
        assert!(
            plain.get("reasoning_effort").is_none(),
            "{proto:?}/{kind:?}"
        );
    }
}

/// The "ignored" cells of the table, as the handler computes them.
#[test]
fn reasoning_ignored_names_what_the_route_cannot_say() {
    fn ignored(p: Protocol, c: &ReasoningControl) -> Vec<&'static str> {
        lmgw_core::proxy::reasoning_ignored(p, c, false)
    }
    let both = ReasoningControl {
        enabled: Some(true),
        effort: None,
        budget_tokens: Some(100),
    };

    // llama-server speaks all three.
    assert!(ignored(Protocol::LlamaCpp, &both).is_empty());
    // A generic OpenAI provider knows `reasoning_effort` and nothing else.
    assert_eq!(ignored(Protocol::Openai, &both), vec!["enabled", "budget"]);
    // An effort expresses "on", so `enabled` is no longer dropped.
    assert_eq!(
        ignored(Protocol::Openai, &effort("high")),
        Vec::<&str>::new()
    );
    assert_eq!(ignored(Protocol::Openai, &enabled(true)), vec!["enabled"]);
    // …but when the client brought OpenRouter's own object, the egress writes
    // `enabled` into it, so claiming it was dropped would be a lie.
    assert_eq!(
        lmgw_core::proxy::reasoning_ignored(Protocol::Openai, &enabled(true), true,),
        Vec::<&str>::new()
    );
    // Anthropic expresses everything — but not a level *and* a budget at once.
    assert!(ignored(Protocol::Anthropic, &both).is_empty());
    assert_eq!(
        ignored(
            Protocol::Anthropic,
            &ReasoningControl {
                enabled: Some(true),
                effort: Some("high".into()),
                budget_tokens: Some(8000),
            }
        ),
        vec!["effort"]
    );
    // Gemini has a budget but no bare on/off, and takes one of level/budget.
    assert_eq!(ignored(Protocol::Gemini, &both), Vec::<&str>::new());
    assert_eq!(ignored(Protocol::Gemini, &enabled(true)), vec!["enabled"]);
    assert_eq!(
        ignored(
            Protocol::Gemini,
            &ReasoningControl {
                enabled: Some(true),
                effort: Some("high".into()),
                budget_tokens: Some(4096),
            }
        ),
        vec!["effort"]
    );
    // Nothing asked for, nothing reported.
    assert!(ignored(Protocol::Gemini, &ReasoningControl::default()).is_empty());
}

// ---------------------------------------------------------------------------
// Shape invariants of the rendered control (review follow-up)
// ---------------------------------------------------------------------------

/// A budget that cannot be a `u32` is refused by name. Truncating
/// `i64::MAX` into a `u32` and adding 1024 would invent a cap out of thin air —
/// the client would get a silently tiny `max_tokens` and no idea why.
#[test]
fn anthropic_refuses_a_budget_that_does_not_fit() {
    let http = reqwest::Client::new();
    let up = kinded(Protocol::Anthropic, UpstreamKind::Generic);
    for absurd in [i64::MAX, (u32::MAX as i64) + 1, -1] {
        let err = for_protocol(Protocol::Anthropic)
            .build_chat(
                &http,
                &up,
                "real-model",
                &sample_ir(),
                &reasoning_params(budget(absurd)),
                false,
            )
            .err()
            .unwrap_or_else(|| panic!("{absurd} should not build"));
        let msg = err.to_string();
        assert!(msg.contains("does not fit"), "{absurd}: {msg}");
        assert!(msg.contains(&absurd.to_string()), "{absurd}: {msg}");
    }
}

/// The largest budget that *does* fit still produces a sane cap rather than
/// wrapping past the end of a `u32`.
#[test]
fn anthropic_saturates_rather_than_wraps_at_the_top_of_u32() {
    let b = anthropic_body(budget(u32::MAX as i64));
    assert_eq!(b["thinking"]["budget_tokens"], u32::MAX as i64);
    assert_eq!(b["max_tokens"], u32::MAX);
}

/// One `thinking` form at a time: the budget says how much to think, so a level
/// beside it would be a second answer to the same question (reported ignored).
#[test]
fn anthropic_never_sends_output_config_beside_a_budget() {
    let b = anthropic_body(ReasoningControl {
        enabled: Some(true),
        effort: Some("high".into()),
        budget_tokens: Some(8000),
    });
    assert_eq!(b["thinking"]["type"], "enabled");
    assert_eq!(b["thinking"]["budget_tokens"], 8000);
    assert!(
        b.get("output_config").is_none(),
        "a level must not ride beside a budget: {b}"
    );

    // `output_config` appears only next to `adaptive`.
    for c in [
        effort("high"),
        enabled(true),
        enabled(false),
        budget(8000),
        ReasoningControl::default(),
    ] {
        let b = anthropic_body(c);
        if b.get("output_config").is_some() {
            assert_eq!(b["thinking"]["type"], "adaptive", "{b}");
        }
    }
}

/// Gemini takes one of `thinkingLevel` / `thinkingBudget`; sending both would
/// leave the provider to guess which the caller meant.
#[test]
fn gemini_never_sends_both_level_and_budget() {
    let b = gemini_body(ReasoningControl {
        enabled: Some(true),
        effort: Some("high".into()),
        budget_tokens: Some(4096),
    });
    let cfg = &b["generationConfig"]["thinkingConfig"];
    assert_eq!(
        cfg["thinkingBudget"], 4096,
        "the budget is the specific one"
    );
    assert!(cfg.get("thinkingLevel").is_none(), "{b}");

    for c in [
        effort("high"),
        enabled(true),
        enabled(false),
        budget(4096),
        ReasoningControl::default(),
    ] {
        let b = gemini_body(c);
        let cfg = &b["generationConfig"]["thinkingConfig"];
        assert!(
            !(cfg.get("thinkingLevel").is_some() && cfg.get("thinkingBudget").is_some()),
            "at most one of thinkingLevel/thinkingBudget: {b}"
        );
    }
}

/// The client's own budget spelling survives *and* agrees with the resolved
/// control — the server must never be handed two different numbers.
#[test]
fn llama_server_reconciles_the_other_budget_spelling() {
    let mut ir = sample_ir();
    ir.passthrough = json!({"thinking_budget_tokens": 2048})
        .as_object()
        .unwrap()
        .clone();
    let up = kinded(Protocol::LlamaCpp, UpstreamKind::LlamaServer);

    // A header raised the budget: both spellings carry the new number.
    let b = reasoning_body(&up, &ir, &reasoning_params(budget(8192)));
    assert_eq!(b["reasoning_budget_tokens"], 8192);
    assert_eq!(b["thinking_budget_tokens"], 8192);

    // Thinking switched off: neither spelling survives to contradict it.
    let b = reasoning_body(&up, &ir, &reasoning_params(enabled(false)));
    assert!(b.get("thinking_budget_tokens").is_none(), "{b}");
    assert!(b.get("reasoning_budget_tokens").is_none(), "{b}");
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
}

// ---------------------------------------------------------------------------
// Usage detail (usage-analytics design §2.3). The IR's `prompt_tokens` is the
// TOTAL input for a turn, cache included; `cached_input_tokens` and
// `cache_write_tokens` are subsets of it. Two of the three dialects already
// report it that way and one does not, so these pin the normalisation.
// ---------------------------------------------------------------------------

#[test]
fn openai_usage_details_are_subsets_of_the_total() {
    let egress = for_protocol(Protocol::Openai);
    let resp = json!({
        "model": "gpt-x",
        "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 1000, "completion_tokens": 200,
            "prompt_tokens_details": {"cached_tokens": 768},
            "completion_tokens_details": {"reasoning_tokens": 150}
        }
    });
    let u = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap()
        .usage;
    assert_eq!(u.prompt_tokens, Some(1000), "total is reported as-is");
    assert_eq!(u.cached_input_tokens, Some(768));
    assert_eq!(
        u.cache_write_tokens, None,
        "OpenAI does not bill cache writes"
    );
    assert_eq!(u.reasoning_tokens, Some(150));
    // 1000 total - 768 cached = 232 billed at the plain input rate.
    assert_eq!(
        u.prompt_tokens.unwrap() - u.cached_input_tokens.unwrap(),
        232
    );
}

#[test]
fn anthropic_disjoint_counters_are_summed_into_the_total() {
    let egress = for_protocol(Protocol::Anthropic);
    let resp = json!({
        "model": "claude-x",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        // Anthropic's three input counters do not overlap.
        "usage": {
            "input_tokens": 100,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 100,
            "output_tokens": 42
        }
    });
    let u = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap()
        .usage;
    assert_eq!(
        u.prompt_tokens,
        Some(1000),
        "100 + 800 + 100: without the sum, a cache-heavy turn reports a tenth \
         of its real input and prices ten times too cheap"
    );
    assert_eq!(u.cached_input_tokens, Some(800));
    assert_eq!(u.cache_write_tokens, Some(100));
    assert_eq!(u.completion_tokens, Some(42));
}

#[test]
fn anthropic_without_cache_fields_is_unchanged() {
    let egress = for_protocol(Protocol::Anthropic);
    let resp = json!({
        "model": "claude-x",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 31, "output_tokens": 9}
    });
    let u = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap()
        .usage;
    assert_eq!(u.prompt_tokens, Some(31));
    assert_eq!(u.cached_input_tokens, None);
    assert_eq!(u.cache_write_tokens, None);
}

#[test]
fn gemini_cached_content_is_a_subset_of_the_prompt_count() {
    let egress = for_protocol(Protocol::Gemini);
    let resp = json!({
        "modelVersion": "gemini-x",
        "candidates": [{"content": {"parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
        "usageMetadata": {
            "promptTokenCount": 1000, "candidatesTokenCount": 50,
            "cachedContentTokenCount": 600, "thoughtsTokenCount": 20
        }
    });
    let u = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap()
        .usage;
    assert_eq!(u.prompt_tokens, Some(1000));
    assert_eq!(u.cached_input_tokens, Some(600));
    assert_eq!(u.reasoning_tokens, Some(20));
}

#[test]
fn usage_merge_is_latest_wins_and_add_sums_the_subsets_too() {
    // merge: streaming chunks report cumulative counts for ONE turn.
    let mut a = Usage {
        prompt_tokens: Some(10),
        completion_tokens: Some(1),
        cached_input_tokens: Some(4),
        ..Default::default()
    };
    a.merge(&Usage {
        prompt_tokens: Some(10),
        completion_tokens: Some(7),
        cached_input_tokens: Some(4),
        ..Default::default()
    });
    assert_eq!(a.completion_tokens, Some(7));
    assert_eq!(a.cached_input_tokens, Some(4), "not 8 — one turn, not two");

    // add: separate upstream calls in a tool loop.
    let mut b = Usage {
        prompt_tokens: Some(10),
        cached_input_tokens: Some(4),
        cache_write_tokens: Some(2),
        reasoning_tokens: Some(3),
        completion_tokens: Some(1),
    };
    b.add(&b.clone());
    assert_eq!(b.prompt_tokens, Some(20));
    assert_eq!(b.cached_input_tokens, Some(8), "paid the cache rate twice");
    assert_eq!(b.cache_write_tokens, Some(4));
    assert_eq!(b.reasoning_tokens, Some(6));
}

#[test]
fn llama_timings_land_on_a_non_streamed_completion() {
    // llama-server puts its timings block on the non-streamed response too;
    // before usage analytics there was no field to receive it.
    let egress = for_protocol(Protocol::Openai);
    let resp = json!({
        "model": "qwen",
        "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 9},
        "timings": {"prompt_n": 120, "prompt_ms": 60.0, "prompt_per_second": 2000.0,
                    "predicted_n": 9, "predicted_ms": 300.0, "predicted_per_second": 30.0,
                    "cache_n": 80, "draft_n": 12, "draft_n_accepted": 9}
    });
    let t = egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap()
        .timings
        .expect("timings parsed");
    assert_eq!(t.prompt_n, 120);
    assert_eq!(t.cache_n, Some(80));
    assert_eq!(t.draft_n_accepted, Some(9));
}

#[test]
fn a_cloud_completion_has_no_timings() {
    let egress = for_protocol(Protocol::Openai);
    let resp = json!({
        "model": "gpt-x",
        "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 9}
    });
    assert!(egress
        .parse_completion(resp.to_string().as_bytes())
        .unwrap()
        .timings
        .is_none());
}
