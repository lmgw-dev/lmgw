//! The OpenAI platform's chat sends on the Responses wire (openai egress
//! design §7.2, decision 16, WP7), end to end against a wiremock standing in
//! for api.openai.com: the Chat tab shows a reasoning summary, a tool loop
//! runs two turns, `/v1/messages` gets a thinking block and then a
//! `tool_use`, `/v1/chat/completions` gets the chat-completions shape with
//! its tool calls, a send with `stop` takes the Chat wire, each answer's
//! items are recorded under the request's key, and a refused summary is
//! learned for the route. The bytes of each body are the corpora's
//! (`egress_golden.rs`, `route_golden.rs`); these check what the routes make
//! of the answers.

use std::time::Duration;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::chat_actions::{post, sse_events};
use crate::chat_golden::{mcp_stub, register_stub, thread};
use crate::common::Gw;
use crate::e2e_proxy::setup_kind;
use crate::stream_usage::row_after;
use crate::support::responses_wire as rw;

/// A platform row in front of a mock whose `/responses` answers with
/// `respond`; the alias `my-model`.
async fn platform<R>(respond: R) -> (MockServer, SharedState, Gw)
where
    R: Fn(&Request) -> ResponseTemplate + Send + Sync + 'static,
{
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(respond)
        .mount(&mock)
        .await;
    let (state, gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    (mock, state, gw)
}

/// The body of each POST the mock got, with its path.
async fn posted(mock: &MockServer) -> Vec<(String, Value)> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| {
            (
                r.url.path().to_string(),
                serde_json::from_slice(&r.body).unwrap_or_default(),
            )
        })
        .collect()
}

fn whole(output: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(rw::response("resp_1", "completed", output))
}

fn streamed(items: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(rw::stream("resp_1", items), "text/event-stream")
}

/// A reasoning item, then a call of `get_weather` for Oslo.
fn thinks_then_calls() -> Vec<Value> {
    vec![
        rw::reasoning("rs_1", "Weighing it."),
        rw::call("fc_1", "call_1", "get_weather", "{\"city\":\"Oslo\"}"),
    ]
}

fn weather_tool_openai() -> Value {
    json!([{"type": "function", "function": {"name": "get_weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}])
}

#[tokio::test]
async fn the_chat_tab_shows_the_reasoning_summary() {
    let (mock, state, gw) = platform(|_: &Request| {
        streamed(&[
            rw::reasoning("rs_1", "Weighing it."),
            rw::message("msg_1", "Hello"),
        ])
    })
    .await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    let text = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await
    .text()
    .await
    .unwrap();
    let events = sse_events(&text);
    assert!(
        events
            .iter()
            .any(|(e, d)| e == "reasoning" && d["text"] == "Weighing it."),
        "{events:?}"
    );
    let reply = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        (reply.content.as_str(), reply.reasoning.as_str()),
        ("Hello", "Weighing it.")
    );
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "/responses");
    assert_eq!(
        recorded(&state).await,
        0,
        "the owner's Chat records nothing"
    );
}

/// The answers recorded for key scoping (`native_responses`). The owner's
/// Chat records none: under `NULL`, the anonymous principal's scope, an
/// anonymous request could send the owner's items back, and the owner's
/// own reader checks none (`proxy::answer_items`).
async fn recorded(state: &SharedState) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM native_responses")
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// The Chat's tool loop: a call, its result sent back as a
/// `function_call_output` beside the call, and the answer after it.
#[tokio::test]
async fn a_tool_loop_runs_two_turns_on_the_responses_wire() {
    let (mock, state, gw) = platform(|req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        match rw::offers(&body, "stub__echo") && !rw::after_tool(&body) {
            true => streamed(&[rw::call("fc_1", "c1", "stub__echo", "{\"text\":\"hi\"}")]),
            false => streamed(&[rw::message("msg_2", "done")]),
        }
    })
    .await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "stub"}]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "say hi through the tool"}),
    )
    .await
    .text()
    .await
    .unwrap();
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "two turns: {sent:?}");
    let input = sent[1].1["input"].as_array().unwrap();
    assert_eq!(input[1]["type"], "function_call", "{input:?}");
    assert_eq!(input[1]["call_id"], "c1");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "c1");
    assert!(
        input[2]["output"].as_str().unwrap().contains("echoed"),
        "{input:?}"
    );
    let reply = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(reply.content.contains("done"), "{reply:?}");
    assert_eq!(
        recorded(&state).await,
        0,
        "the owner's turns record nothing"
    );
}

#[tokio::test]
async fn messages_gets_a_thinking_block_then_tool_use() {
    let (_mock, _state, gw) = platform(|req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        match body["stream"] == true {
            true => streamed(&thinks_then_calls()),
            false => whole(json!(thinks_then_calls())),
        }
    })
    .await;
    let ask = |stream: bool| {
        json!({"model": "my-model", "max_tokens": 256, "stream": stream,
               "messages": [{"role": "user", "content": "Weather in Oslo?"}],
               "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]})
    };
    let v: Value = post(&gw, "/v1/messages", ask(false))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["content"][0]["type"], "thinking", "{v}");
    assert_eq!(v["content"][0]["thinking"], "Weighing it.", "{v}");
    assert_eq!(v["content"][1]["type"], "tool_use", "{v}");
    assert_eq!(v["content"][1]["id"], "call_1", "{v}");
    assert_eq!(v["content"][1]["name"], "get_weather", "{v}");
    assert_eq!(v["content"][1]["input"], json!({"city": "Oslo"}), "{v}");
    assert_eq!(v["stop_reason"], "tool_use", "{v}");

    let text = post(&gw, "/v1/messages", ask(true))
        .await
        .text()
        .await
        .unwrap();
    let starts: Vec<String> = sse_events(&text)
        .into_iter()
        .filter(|(e, _)| e == "content_block_start")
        .map(|(_, d)| d["content_block"]["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(starts, ["thinking", "tool_use"], "{text}");
    assert!(text.contains("\"stop_reason\":\"tool_use\""), "{text}");
}

#[tokio::test]
async fn chat_completions_gets_the_chat_shape_with_its_tool_calls() {
    let (_mock, _state, gw) = platform(|req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        match body["stream"] == true {
            true => streamed(&thinks_then_calls()),
            false => whole(json!(thinks_then_calls())),
        }
    })
    .await;
    let ask = |stream: bool| {
        json!({"model": "my-model", "stream": stream, "tools": weather_tool_openai(),
               "messages": [{"role": "user", "content": "Weather in Oslo?"}]})
    };
    let v: Value = post(&gw, "/v1/chat/completions", ask(false))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["object"], "chat.completion", "{v}");
    let choice = &v["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls", "{v}");
    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["id"], "call_1", "{v}");
    assert_eq!(call["type"], "function", "{v}");
    assert_eq!(call["function"]["name"], "get_weather", "{v}");
    let args: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({"city": "Oslo"}));
    assert_eq!(v["service_tier"], "default", "{v}");

    let text = post(&gw, "/v1/chat/completions", ask(true))
        .await
        .text()
        .await
        .unwrap();
    let chunks: Vec<Value> = text
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    assert!(
        chunks
            .iter()
            .all(|c| c["object"] == "chat.completion.chunk"),
        "{text}"
    );
    let calls: Vec<&Value> = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"].get(0))
        .collect();
    assert_eq!(calls[0]["id"], "call_1", "{text}");
    assert_eq!(calls[0]["function"]["name"], "get_weather", "{text}");
    assert!(
        chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "tool_calls"),
        "{text}"
    );
}

/// `stop` has no Responses spelling: the send takes the Chat wire, which
/// writes it, and nothing goes to `/responses`.
#[tokio::test]
async fn a_send_with_stop_takes_the_chat_wire() {
    let (mock, _state, gw) = platform(|_: &Request| ResponseTemplate::new(500)).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
        })))
        .mount(&mock)
        .await;
    let v: Value = post(
        &gw,
        "/v1/chat/completions",
        json!({"model": "my-model", "stop": ["END"],
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "ok", "{v}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, "/chat/completions");
    assert_eq!(sent[0].1["stop"], json!(["END"]));
}

/// A cap below the Responses API's minimum of 16 (a `/v1/messages` token
/// probe's `max_tokens: 1`) takes the Chat wire, which takes it, and goes
/// as the caller sent it, not raised.
#[tokio::test]
async fn a_cap_below_sixteen_takes_the_chat_wire_as_it_is() {
    let (mock, _state, gw) = platform(|_: &Request| ResponseTemplate::new(500)).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "o"},
                         "finish_reason": "length"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
        })))
        .mount(&mock)
        .await;
    let resp = post(
        &gw,
        "/v1/messages",
        json!({"model": "my-model", "max_tokens": 1,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, "/chat/completions");
    assert_eq!(sent[0].1["max_completion_tokens"], 1, "{}", sent[0].1);
}

/// A client key `name`, its bearer `lmgw-<name>`; its id.
async fn client_key(state: &SharedState, name: &str) -> i64 {
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES (?1, ?2, 1)")
        .bind(name)
        .bind(lmgw_core::config::hash_api_key(&format!("lmgw-{name}")))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    sqlx::query_scalar("SELECT id FROM api_keys WHERE name = ?1")
        .bind(name)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// The answer's response id and output item ids are recorded under the key
/// that got them, not stored (`store: false`), before the answer reaches
/// its client: whole, and streamed.
#[tokio::test]
async fn the_answer_s_items_are_recorded_under_its_key() {
    let (_mock, state, gw) = platform(|req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let items = [rw::reasoning("rs_1", "Hm."), rw::message("msg_1", "Hello")];
        match body["stream"] == true {
            true => streamed(&items),
            false => whole(json!(items)),
        }
    })
    .await;
    let key = client_key(&state, "k1").await;
    for stream in [false, true] {
        sqlx::query("DELETE FROM native_responses")
            .execute(&state.db)
            .await
            .unwrap();
        let text = gw
            .anon()
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("lmgw-k1")
            .json(&json!({"model": "my-model", "stream": stream,
                          "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(text.contains("Hello"), "{text}");
        let recorded: Vec<(String, Option<i64>, bool, String)> =
            sqlx::query_as("SELECT id, key_id, stored, model FROM native_responses ORDER BY id")
                .fetch_all(&state.db)
                .await
                .unwrap();
        assert_eq!(
            recorded,
            [(
                "resp_1".to_string(),
                Some(key),
                false,
                "my-model".to_string()
            )],
            "{stream}"
        );
        let items = ["rs_1", "msg_1"].map(store::ItemClaim::id);
        assert_eq!(
            store::first_item_not_of(&state.db, &items, Some(key))
                .await
                .unwrap(),
            None,
            "{stream}: the key's own"
        );
        assert_eq!(
            store::first_item_not_of(&state.db, &items, None)
                .await
                .unwrap()
                .as_deref(),
            Some("rs_1"),
            "{stream}: no other key's"
        );
    }
}

/// A refusal of the reasoning summary is learned for the route: the send
/// goes again without it, the refused attempt keeps its own row, and the
/// next send asks for none.
#[tokio::test]
async fn a_refused_summary_is_learned_for_the_route() {
    let (mock, state, gw) = platform(|req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        if body.pointer("/reasoning/summary").is_some() {
            return ResponseTemplate::new(400).set_body_json(json!({"error": {
                "message": "Your organization must be verified to generate reasoning summaries.",
                "type": "invalid_request_error", "param": "reasoning.summary",
                "code": "unsupported_value"}}));
        }
        whole(json!([rw::message("msg_1", "Hello")]))
    })
    .await;
    let ask = json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]});
    for n in 0..2 {
        let v: Value = post(&gw, "/v1/chat/completions", ask.clone())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "Hello", "{n}: {v}");
    }
    let sent = posted(&mock).await;
    let summaries: Vec<bool> = sent
        .iter()
        .map(|(_, b)| b.pointer("/reasoning/summary").is_some())
        .collect();
    assert_eq!(summaries, [true, false, false], "asked once");
    let refused = row_after(&state, 2).await;
    let rows = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "{rows:#?}");
    let first = rows.iter().min_by_key(|r| r.id).unwrap();
    assert_eq!(first.status, 400, "{first:?}");
    assert!(
        first
            .error_msg
            .as_deref()
            .unwrap()
            .contains("asks for none on this model from now on"),
        "{first:?}"
    );
    assert_eq!(refused.status, 200);
}

/// OpenAI's refusals of a send that needs both wires (§7.2.5, the owner's
/// answers of 2026-10-24 and 2026-10-25), with the bodies the 2026-10-25
/// probe got (`tests/fixtures/openai_platform/`): GPT-6 refuses `stop`
/// itself, and gpt-6-astra refuses function tools on the Chat wire naming
/// `reasoning_effort`, though lmgw sent none. Each reaches the client as
/// OpenAI wrote it, after one send: lmgw neither retries nor sets
/// `reasoning_effort: "none"` on the client's behalf.
#[tokio::test]
async fn a_chat_wire_refusal_reaches_the_client_as_openai_wrote_it() {
    for fixture in ["astra_chat_tools_refused", "stop_unsupported"] {
        let raw = std::fs::read_to_string(format!(
            "{}/tests/fixtures/openai_platform/{fixture}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let refusal: Value = serde_json::from_str(&raw).unwrap();
        let (mock, _state, gw) = platform(|_: &Request| ResponseTemplate::new(500)).await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(raw.clone(), "application/json"))
            .mount(&mock)
            .await;
        let resp = post(
            &gw,
            "/v1/chat/completions",
            json!({"model": "my-model", "stop": ["END"], "tools": weather_tool_openai(),
                   "messages": [{"role": "user", "content": "Weather in Graz?"}]}),
        )
        .await;
        assert_eq!(resp.status(), 400, "{fixture}");
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["error"], refusal["error"], "{fixture}: {v}");
        let sent = posted(&mock).await;
        assert_eq!(sent.len(), 1, "{fixture}: one send, no retry: {sent:?}");
        assert!(sent[0].1.get("reasoning_effort").is_none(), "{}", sent[0].1);
    }
}
