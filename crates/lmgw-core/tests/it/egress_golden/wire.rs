//! Everything beside the chat body (§9.1): embeddings and rerank (request
//! and response), the count plan and its `/tokenize` request, the count's
//! response, `map_error` on recorded error bodies (the context refusal
//! included), the deltas decoded from recorded SSE, and whole completions.

use lmgw_core::config::Upstream;
use lmgw_core::egress::{for_protocol, CountPlan};
use lmgw_core::error::GatewayError;
use lmgw_core::ir::{EmbeddingsRequest, RerankRequest, StreamDelta};
use lmgw_core::sse::SseDecoder;
use serde_json::{json, Value};

use super::{exceed_context, http, keyless, rebased, request_json, Case, MODEL};

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

fn error(e: &GatewayError) -> Value {
    json!(format!("{e:?}"))
}

fn plan_json(plan: Result<CountPlan, GatewayError>) -> Value {
    match plan {
        Ok(CountPlan::Ready(n)) => json!({"ready": n}),
        Ok(CountPlan::Guessed(n)) => json!({"guessed": n}),
        Ok(CountPlan::Request(rb)) => json!({"request": request_json(*rb)}),
        Err(e) => json!({"error": error(&e)}),
    }
}

fn count_plan(up: &Upstream, model: &str, text: &str) -> Value {
    plan_json(for_protocol(up.protocol).build_count_tokens(http(), up, model, text))
}

fn delta_json(d: &StreamDelta) -> Value {
    match d {
        StreamDelta::TextDelta(t) => json!({"text": t}),
        StreamDelta::ReasoningDelta(t) => json!({"reasoning": t}),
        StreamDelta::ToolCallStart { index, id, name } => {
            json!({"tool_call_start": {"index": index, "id": id, "name": name}})
        }
        StreamDelta::ToolCallArgsDelta { index, fragment } => {
            json!({"tool_call_args": {"index": index, "fragment": fragment}})
        }
        StreamDelta::Usage(u) => json!({"usage": u}),
        StreamDelta::Stop(f) => json!({"stop": f}),
        StreamDelta::Timings(t) => json!({"timings": t}),
        StreamDelta::Error(e) => json!({"error": e}),
    }
}

/// `sse` through the SSE framer and `up`'s egress decoder: the deltas, one
/// list per event.
fn decode(up: &Upstream, sse: &str) -> Value {
    let mut framer = SseDecoder::new();
    let mut dec = for_protocol(up.protocol).new_decoder();
    let events: Vec<Value> = framer
        .feed(sse.as_bytes())
        .iter()
        .map(|ev| Value::Array(dec.on_event(ev).iter().map(delta_json).collect()))
        .collect();
    json!({ "deltas": events })
}

fn completion(up: &Upstream, body: &str) -> Value {
    match for_protocol(up.protocol).parse_completion(body.as_bytes()) {
        Ok(c) => serde_json::to_value(&c).unwrap(),
        Err(e) => json!({"error": error(&e)}),
    }
}

fn embeddings_parsed(up: &Upstream, body: &str) -> Value {
    match for_protocol(up.protocol).parse_embeddings(body.as_bytes()) {
        Ok(r) => json!({"embeddings": r.embeddings, "usage": r.usage, "model": r.model}),
        Err(e) => json!({"error": error(&e)}),
    }
}

fn rerank_parsed(up: &Upstream, body: &str) -> Value {
    match for_protocol(up.protocol).parse_rerank(body.as_bytes()) {
        Ok(r) => json!({
            "results": r.results.iter()
                .map(|s| json!({"index": s.index, "score": s.score}))
                .collect::<Vec<_>>(),
            "usage": r.usage,
            "model": r.model,
        }),
        Err(e) => json!({"error": error(&e)}),
    }
}

// ---------------------------------------------------------------------------
// Recorded bodies
// ---------------------------------------------------------------------------

const TIMINGS: &str = r#"{"cache_n":3,"prompt_n":17,"prompt_ms":21.5,"prompt_per_token_ms":1.26,"prompt_per_second":790.7,"predicted_n":12,"predicted_ms":96.0,"predicted_per_token_ms":8.0,"predicted_per_second":125.0,"draft_n":6,"draft_n_accepted":4}"#;

/// A llama-server stream: reasoning, text, two tool calls (one with its
/// arguments in fragments), the finish, then usage and timings.
fn llama_stream() -> String {
    [
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":"Let me"}}],"created":1,"id":"chatcmpl-x","model":"gguf","system_fingerprint":"b1-abc","object":"chat.completion.chunk"}"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"reasoning_content":" think."}}],"object":"chat.completion.chunk"}"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"Checking."}}],"object":"chat.completion.chunk"}"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"get_weather","arguments":""}}]}}],"object":"chat.completion.chunk"}"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}],"object":"chat.completion.chunk"}"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Berlin\"}"}}]}}],"object":"chat.completion.chunk"}"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"get_time","arguments":"{}"}}]}}],"object":"chat.completion.chunk"}"#,
        &format!(r#"data: {{"choices":[{{"finish_reason":"tool_calls","index":0,"delta":{{}}}}],"object":"chat.completion.chunk","timings":{TIMINGS}}}"#),
        &format!(r#"data: {{"choices":[],"object":"chat.completion.chunk","usage":{{"completion_tokens":12,"prompt_tokens":20,"total_tokens":32}},"timings":{TIMINGS}}}"#),
        "data: [DONE]",
    ]
    .map(|l| format!("{l}\n\n"))
    .concat()
}

pub(super) fn cases() -> Vec<Case> {
    vec![
        // --- Embeddings ----------------------------------------------------
        Case {
            name: "embeddings",
            run: |up| {
                let req = EmbeddingsRequest {
                    model_alias: "alias".into(),
                    inputs: vec!["alpha".into(), "beta".into()],
                    dimensions: None,
                };
                let rb = for_protocol(up.protocol)
                    .build_embeddings(http(), up, MODEL, &req)
                    .unwrap();
                json!({"request": request_json(rb)})
            },
        },
        Case {
            name: "embeddings_dimensions",
            run: |up| {
                let req = EmbeddingsRequest {
                    model_alias: "alias".into(),
                    inputs: vec!["alpha".into()],
                    dimensions: Some(256),
                };
                let rb = for_protocol(up.protocol)
                    .build_embeddings(http(), &keyless(up), MODEL, &req)
                    .unwrap();
                json!({"request": request_json(rb)})
            },
        },
        Case {
            name: "embeddings_responses",
            run: |up| {
                let bodies = [
                    r#"{"object":"list","model":"gguf","data":[{"object":"embedding","index":0,"embedding":[0.5,-0.25,0.125]},{"object":"embedding","index":1,"embedding":[1,0,-1]}],"usage":{"prompt_tokens":4,"total_tokens":4}}"#,
                    r#"{"data":[]}"#,
                    r#"{"model":"m"}"#,
                    "not json",
                ];
                json!({"parsed": bodies.iter().map(|b| json!({
                    "body": b, "parsed": embeddings_parsed(up, b),
                })).collect::<Vec<_>>()})
            },
        },
        // --- Rerank --------------------------------------------------------
        Case {
            name: "rerank",
            run: |up| {
                let req = RerankRequest {
                    model_alias: "alias".into(),
                    query: "capital of France".into(),
                    documents: vec!["Paris".into(), "Berlin".into(), "Rome".into()],
                    top_n: None,
                };
                let rb = for_protocol(up.protocol)
                    .build_rerank(http(), up, MODEL, &req)
                    .unwrap();
                json!({"request": request_json(rb)})
            },
        },
        Case {
            name: "rerank_top_n",
            run: |up| {
                let req = RerankRequest {
                    model_alias: "alias".into(),
                    query: "capital of France".into(),
                    documents: vec!["Paris".into(), "Berlin".into(), "Rome".into()],
                    top_n: Some(2),
                };
                let rb = for_protocol(up.protocol)
                    .build_rerank(http(), up, MODEL, &req)
                    .unwrap();
                json!({"request": request_json(rb)})
            },
        },
        Case {
            name: "rerank_responses",
            run: |up| {
                let bodies = [
                    // Jina, as llama-server answers.
                    r#"{"model":"gguf","object":"list","usage":{"prompt_tokens":9,"total_tokens":9},"results":[{"index":2,"relevance_score":-3.5},{"index":0,"relevance_score":7.25}]}"#,
                    // TEI's bare array.
                    r#"[{"index":1,"score":0.75},{"index":0,"score":0.5}]"#,
                    r#"{"data":[]}"#,
                    "not json",
                ];
                json!({"parsed": bodies.iter().map(|b| json!({
                    "body": b, "parsed": rerank_parsed(up, b),
                })).collect::<Vec<_>>()})
            },
        },
        // --- The count -----------------------------------------------------
        Case {
            name: "count_plan",
            run: |up| json!({"plan": count_plan(up, MODEL, "Hello, world! How are you?")}),
        },
        Case {
            name: "count_plan_known_model",
            run: |up| json!({"plan": count_plan(up, "gpt-4o", "Hello, world! How are you?")}),
        },
        Case {
            name: "count_plan_roots",
            run: |up| {
                let roots: Vec<Value> = ["", "/v1/", "/llama/v1", "/v1/v1"]
                    .iter()
                    .map(|path| {
                        let up = rebased(up, path);
                        json!({"base_url": up.base_url, "plan": count_plan(&up, MODEL, "hi")})
                    })
                    .collect();
                json!({"plans": roots, "keyless": count_plan(&keyless(up), MODEL, "hi")})
            },
        },
        Case {
            name: "count_responses",
            run: |up| {
                let bodies = [
                    r#"{"tokens":[1,15043,29892,3186]}"#,
                    r#"{"tokens":[{"id":1,"piece":"<s>"},{"id":15043,"piece":"Hello"}]}"#,
                    r#"{"tokens":[]}"#,
                    r#"{"count":3}"#,
                    "not json",
                ];
                let egress = for_protocol(up.protocol);
                json!({"parsed": bodies.iter().map(|b| json!({
                    "body": b,
                    "parsed": match egress.parse_count(b.as_bytes()) {
                        Ok(n) => json!(n),
                        Err(e) => json!({"error": error(&e)}),
                    },
                })).collect::<Vec<_>>()})
            },
        },
        // --- Errors --------------------------------------------------------
        Case {
            name: "errors",
            run: |up| {
                let long = "x".repeat(520);
                let bodies: Vec<(u16, String)> = vec![
                    // llama-server's context refusal, verbatim shape.
                    (400, r#"{"error":{"code":400,"message":"request (8010 tokens) exceeds the available context size (4096 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":8010,"n_ctx":4096}}"#.into()),
                    // The same body under another status is not that refusal.
                    (500, r#"{"error":{"code":400,"message":"request (8010 tokens) exceeds the available context size (4096 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":8010,"n_ctx":4096}}"#.into()),
                    // Without its numbers it is not either.
                    (400, r#"{"error":{"code":400,"message":"too long","type":"exceed_context_size_error","n_prompt_tokens":8010}}"#.into()),
                    // The shared pool's overflow.
                    (500, r#"{"error":{"code":500,"message":"Context size has been exceeded.","type":"server_error"}}"#.into()),
                    // A media refusal.
                    (500, r#"{"error":{"code":500,"message":"image input is not supported - hint: if this is unexpected, you may need to provide the mmproj","type":"server_error"}}"#.into()),
                    (503, r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#.into()),
                    // A cloud provider's 400, 401 and 404.
                    (400, r#"{"error":{"message":"Invalid value for 'temperature'.","type":"invalid_request_error","param":"temperature","code":null}}"#.into()),
                    (401, r#"{"error":{"message":"Incorrect API key provided.","type":"invalid_request_error","code":"invalid_api_key"}}"#.into()),
                    (404, r#"{"error":{"message":"The model 'x' does not exist","type":"invalid_request_error","code":"model_not_found"}}"#.into()),
                    (429, r#"{"error":"rate limited"}"#.into()),
                    (502, "Bad Gateway".into()),
                    (400, String::new()),
                    (500, long),
                ];
                let egress = for_protocol(up.protocol);
                json!({"errors": bodies.iter().map(|(status, body)| json!({
                    "status": status,
                    "body": body,
                    "map_error": error(&egress.map_error(*status, body.as_bytes())),
                    "exceed_context": exceed_context(body.as_bytes()),
                })).collect::<Vec<_>>()})
            },
        },
        // --- Streams -------------------------------------------------------
        Case {
            name: "stream_llama",
            run: |up| decode(up, &llama_stream()),
        },
        Case {
            name: "stream_timings_per_token",
            run: |up| {
                let chunk = |delta: &str, n: u32| {
                    format!(
                        r#"data: {{"choices":[{{"finish_reason":null,"index":0,"delta":{delta}}}],"timings":{{"prompt_n":5,"prompt_ms":10.0,"prompt_per_second":500.0,"predicted_n":{n},"predicted_ms":{ms}.0,"predicted_per_second":100.0}}}}"#,
                        ms = n * 10
                    )
                };
                let sse = [
                    chunk(r#"{"content":"Hel"}"#, 1),
                    chunk(r#"{"content":"lo"}"#, 2),
                    r#"data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"timings":{"prompt_n":5,"prompt_ms":10.0,"prompt_per_second":500.0,"predicted_n":2,"predicted_ms":20.0,"predicted_per_second":100.0}}"#.into(),
                    "data: [DONE]".into(),
                ]
                .map(|l| format!("{l}\n\n"))
                .concat();
                decode(up, &sse)
            },
        },
        Case {
            name: "stream_openrouter",
            run: |up| {
                let sse = concat!(
                    ": OPENROUTER PROCESSING\n\n",
                    r#"data: {"id":"gen-1","provider":"x","model":"vendor/model","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":"Weighing it.","reasoning_details":[{"type":"reasoning.text","text":"Weighing it."}]},"finish_reason":null}]}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{"content":"Blue light","reasoning":null},"finish_reason":null}]}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{"content":" scatters."},"finish_reason":"stop","native_finish_reason":"stop"}]}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{"content":""},"finish_reason":null}],"usage":{"prompt_tokens":30,"completion_tokens":40,"total_tokens":70,"prompt_tokens_details":{"cached_tokens":10},"completion_tokens_details":{"reasoning_tokens":25}}}"#,
                    "\n\n",
                    "data: [DONE]\n\n",
                );
                decode(up, sse)
            },
        },
        Case {
            name: "stream_error",
            run: |up| {
                let sse = concat!(
                    r#"data: {"choices":[{"index":0,"delta":{"content":"Partial"}}]}"#,
                    "\n\n",
                    r#"data: {"error":{"code":500,"message":"the slot was cancelled","type":"server_error"}}"#,
                    "\n\n",
                    r#"data: {"error":{"code":500}}"#,
                    "\n\n",
                );
                decode(up, sse)
            },
        },
        Case {
            name: "stream_edges",
            run: |up| {
                let sse = concat!(
                    "data: \n\n",
                    "data: {not json\n\n",
                    r#"data: {"choices":[]}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":3,"function":{"name":"","arguments":"{\"a\""}}]}}]}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":3,"function":{"name":"lookup","arguments":":1}"}}]}}]}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":3,"function":{"name":"lookup","arguments":""}}]}}]}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"length"}],"usage":null}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}],"timings":{"predicted_n":1}}"#,
                    "\n\n",
                    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"eos_token"}]}"#,
                    "\n\n",
                    "event: ping\ndata: [DONE]\n\n",
                );
                decode(up, sse)
            },
        },
        // --- Whole completions ---------------------------------------------
        Case {
            name: "completion_llama",
            run: |up| {
                let body = format!(
                    r#"{{"choices":[{{"finish_reason":"tool_calls","index":0,"message":{{"role":"assistant","content":"Checking.","reasoning_content":"Need the weather.","tool_calls":[{{"type":"function","function":{{"name":"get_weather","arguments":"{{\"city\":\"Berlin\"}}"}},"id":"call_a"}}]}}}}],"created":1,"model":"gguf","system_fingerprint":"b1-abc","object":"chat.completion","usage":{{"completion_tokens":12,"prompt_tokens":20,"total_tokens":32,"prompt_tokens_details":{{"cached_tokens":3}}}},"id":"chatcmpl-x","timings":{TIMINGS}}}"#
                );
                json!({"completion": completion(up, &body)})
            },
        },
        Case {
            name: "completion_openrouter",
            run: |up| {
                let body = r#"{"id":"gen-1","model":"vendor/model","object":"chat.completion","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"Blue light scatters.","reasoning":"Weighing it."}}],"usage":{"prompt_tokens":30,"completion_tokens":40,"total_tokens":70,"prompt_tokens_details":{"cached_tokens":10},"completion_tokens_details":{"reasoning_tokens":25}}}"#;
                json!({"completion": completion(up, body)})
            },
        },
        Case {
            name: "completion_edges",
            run: |up| {
                let bodies = [
                    r#"{"choices":[{"message":{"content":"","tool_calls":[{"id":"c","function":{"name":"f","arguments":"not json"}}]},"finish_reason":"length"}]}"#,
                    r#"{"choices":[{"message":{"content":"hi"}}],"timings":{"predicted_n":1}}"#,
                    r#"{"choices":[{"message":{"content":null},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":1}}"#,
                    r#"{"choices":[]}"#,
                    "not json",
                ];
                json!({"parsed": bodies.iter().map(|b| json!({
                    "body": b, "parsed": completion(up, b),
                })).collect::<Vec<_>>()})
            },
        },
    ]
}
