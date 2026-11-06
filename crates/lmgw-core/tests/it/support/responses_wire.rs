//! OpenAI's Responses wire as a wiremock speaks it, for the platform's chat
//! sends (openai egress design §7.2, WP7): whole Response objects and
//! hand-written streams, until the live probe (WP6) captures real ones.

use serde_json::{json, Value};

/// `events` as the SSE body the platform streams, each with its `event:`
/// line and its `type`.
pub fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// A Response object of `output` with `usage` 7 in and 1 out (`id`
/// `resp_1`), the tier `default`.
pub fn response(id: &str, status: &str, output: Value) -> Value {
    json!({
        "id": id, "object": "response", "created_at": 0, "status": status,
        "model": "chat-gguf", "service_tier": "default", "output": output,
        "usage": {"input_tokens": 7, "output_tokens": 1, "total_tokens": 8},
    })
}

/// A message output item saying `text`.
pub fn message(id: &str, text: &str) -> Value {
    json!({"type": "message", "id": id, "role": "assistant", "status": "completed",
           "content": [{"type": "output_text", "text": text, "annotations": []}]})
}

/// A message output item whose one part is the model's refusal `text`.
pub fn refusal(id: &str, text: &str) -> Value {
    json!({"type": "message", "id": id, "role": "assistant", "status": "completed",
           "content": [{"type": "refusal", "refusal": text}]})
}

/// A reasoning output item whose summary is `summary`.
pub fn reasoning(id: &str, summary: &str) -> Value {
    json!({"type": "reasoning", "id": id,
           "summary": [{"type": "summary_text", "text": summary}],
           "encrypted_content": "gAAAAB-sealed"})
}

/// A function call output item.
pub fn call(id: &str, call_id: &str, name: &str, args: &str) -> Value {
    json!({"type": "function_call", "id": id, "call_id": call_id, "name": name,
           "arguments": args, "status": "completed"})
}

/// The stream of the Response `id` whose `output` is `items`, each item's
/// events in order (a reasoning summary, text, a call's arguments), ended by
/// `response.completed` carrying every item. `response.created` echoes the
/// tier `auto`, as the platform's does.
pub fn stream(id: &str, items: &[Value]) -> String {
    let mut events = vec![json!({"type": "response.created", "response": {
        "id": id, "object": "response", "status": "in_progress",
        "service_tier": "auto", "output": []}})];
    for (i, it) in items.iter().enumerate() {
        let added = match it["type"].as_str() {
            Some("function_call") => {
                let mut a = it.clone();
                a["arguments"] = json!("");
                a
            }
            _ => json!({"type": it["type"], "id": it["id"]}),
        };
        events.push(
            json!({"type": "response.output_item.added", "output_index": i,
                           "item": added}),
        );
        match it["type"].as_str() {
            Some("reasoning") => events.push(json!({
                "type": "response.reasoning_summary_text.delta", "output_index": i,
                "summary_index": 0, "item_id": it["id"],
                "delta": it["summary"][0]["text"]})),
            Some("message") if it["content"][0]["type"] == "refusal" => events.push(json!({
                "type": "response.refusal.delta", "output_index": i,
                "content_index": 0, "item_id": it["id"],
                "delta": it["content"][0]["refusal"]})),
            Some("message") => events.push(json!({
                "type": "response.output_text.delta", "output_index": i,
                "content_index": 0, "item_id": it["id"],
                "delta": it["content"][0]["text"]})),
            Some("function_call") => events.push(json!({
                "type": "response.function_call_arguments.delta", "output_index": i,
                "item_id": it["id"], "delta": it["arguments"]})),
            _ => {}
        }
        events.push(
            json!({"type": "response.output_item.done", "output_index": i,
                           "item": it}),
        );
    }
    events.push(json!({"type": "response.completed",
                       "response": response(id, "completed", Value::Array(items.to_vec()))}));
    sse(&events)
}

/// Whether a Responses body's `input` ends in a tool result.
pub fn after_tool(body: &Value) -> bool {
    body["input"]
        .as_array()
        .and_then(|i| i.last())
        .is_some_and(|i| i["type"] == "function_call_output")
}

/// Mount on `mock` the platform's answer `text` to every Responses-wire
/// send: a stream when one is asked, else the whole Response.
pub async fn mount_reply(mock: &wiremock::MockServer, text: &str) {
    let text = text.to_string();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let item = message("msg_1", &text);
            match body["stream"] == true {
                true => wiremock::ResponseTemplate::new(200)
                    .set_body_raw(stream("resp_1", &[item]), "text/event-stream"),
                false => wiremock::ResponseTemplate::new(200).set_body_json(response(
                    "resp_1",
                    "completed",
                    json!([item]),
                )),
            }
        })
        .mount(mock)
        .await;
}

/// Whether a Responses body offers the function tool `name`.
pub fn offers(body: &Value, name: &str) -> bool {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|t| t["name"] == name)
}
