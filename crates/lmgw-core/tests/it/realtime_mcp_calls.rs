//! Server-side MCP calls on `/v1/realtime`, text mode (realtime-server-tools
//! design §2.1–§2.4, §3, §4; WP3): the round trip as `@openai/agents` makes
//! it — the golden event order, every `mcp_call` with all its fields, the
//! follow-up that renders call and result — the rows the calls leave, a
//! tool's error and a failed call, a function and a server call in one turn,
//! `parallel_tool_calls: false`, and the responses that run nothing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lmgw_core::config::KeyPolicy;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::mcp_stub::{answer, answering, echo_stub, register, McpStub};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, send, types, user_text, Step, Turn, Ws, KEY,
};
use crate::support::realtime_mcp::{
    assert_every_mcp_call, assert_mcp_call, at, calls, rows, shape, tools_session,
};

/// The `mcp` tool as `@openai/agents`' `hostedMcpTool` declares it, for
/// label `a`.
fn hosted_a() -> Value {
    json!([{"type": "mcp", "server_label": "a", "server_url": "https://example.invalid/mcp",
            "allowed_tools": {"tool_names": ["echo", "fail", "boom"]},
            "require_approval": "never"}])
}

/// The registered server `alpha` (tool prefix `a`) behind `stub`.
async fn alpha(state: &SharedState, stub: McpStub) -> McpStub {
    register(state, "alpha", "a", &stub.url, true, None).await;
    stub
}

/// `echo` alone.
fn echo_tools() -> Value {
    json!([{"name": "echo", "description": "echo the input",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}])
}

/// A user turn, and `response.create`.
async fn ask(ws: &mut Ws, text: &str) {
    send(ws, user_text(text)).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, json!({"type": "response.create"})).await;
}

/// §5's golden sequence: the call's item and arguments, its run, its done
/// events, then `response.done` carrying it with its `output` — every
/// `mcp_call` with every field. The SDK's follow-up, sent on the call's
/// `conversation.item.done`, is queued behind it and renders the call and
/// its result.
#[tokio::test]
async fn a_text_round_trip_is_the_golden_sequence() {
    let (log, _guard) = crate::common::captured_log::capture_log();
    let fake = chat_fake().await;
    fake.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_m1"),
            name: "a__echo",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"text":"#,
        },
        Step::CallArgs {
            index: 0,
            args: r#""hi"}"#,
        },
        Step::Finish("tool_calls"),
        Step::Usage(6, 4),
    ]));
    fake.push(Turn::text(&["It ", "says hi."]));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let stub = alpha(&state, echo_stub().await).await;
    let mut ws = tools_session(&addr, None, hosted_a(), 1).await;
    send(&mut ws, user_text("echo hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "create_1"}),
    )
    .await;

    let mut events = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(
        types(&events),
        [
            "response.created",
            "response.output_item.added",
            "conversation.item.added",
            "response.mcp_call_arguments.delta",
            "response.mcp_call_arguments.delta",
            "response.mcp_call_arguments.done",
            "response.mcp_call.in_progress",
            "response.mcp_call.completed",
            "response.output_item.done",
            "conversation.item.done",
        ]
    );
    let rid = events[0]["response"]["id"].as_str().unwrap().to_string();
    let added = &events[1]["item"];
    assert_mcp_call(added);
    assert_eq!(
        (&added["server_label"], &added["name"], &added["arguments"]),
        (&json!("a"), &json!("echo"), &json!(""))
    );
    for key in ["output", "error", "approval_request_id"] {
        assert_eq!(added[key], Value::Null, "{key}: {added}");
    }
    for key in ["call_id", "status"] {
        assert!(added.get(key).is_none(), "{key}: {added}");
    }
    let id = added["id"].as_str().unwrap().to_string();
    assert_eq!(events[2]["item"], *added);
    for d in &events[3..5] {
        assert_eq!(
            (d["response_id"].as_str(), d["item_id"].as_str()),
            (Some(rid.as_str()), Some(id.as_str())),
            "{d}"
        );
        assert_eq!(d["output_index"], 0, "{d}");
    }
    assert_eq!(events[5]["arguments"], r#"{"text":"hi"}"#);
    for e in &events[6..8] {
        assert_eq!(e["item_id"], id.as_str(), "{e}");
        assert_eq!(e["output_index"], 0, "{e}");
    }
    let done = events[8]["item"].clone();
    assert_eq!(done["output"], "echo: hi");
    assert_eq!(done["error"], Value::Null);
    assert_eq!(done["arguments"], r#"{"text":"hi"}"#);
    assert_eq!(events[9]["item"], done);
    assert_eq!(
        stub.calls(),
        vec![("echo".to_string(), json!({"text": "hi"}))]
    );

    // The SDK's automatic follow-up, at once.
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "create_2"}),
    )
    .await;
    events.extend(events_until(&mut ws, "response.done").await);
    let first = events.last().unwrap()["response"].clone();
    assert_eq!(first["status"], "completed");
    assert_eq!(first["output"], json!([done]));
    events.extend(events_until(&mut ws, "response.done").await);
    assert!(
        !events.iter().any(|e| e["type"] == "error"),
        "no event was refused: {events:#?}"
    );
    assert_every_mcp_call(&events);
    let second = &events.last().unwrap()["response"];
    assert_eq!(second["output"][0]["content"][0]["text"], "It says hi.");

    // Offered as a function after nothing else, by its exposed name.
    let body = fake.seen.chat(0);
    assert_eq!(body["tools"][0]["function"]["name"], "a__echo", "{body}");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(
        shape(&fake.seen.chat(1)),
        [
            ("user".to_string(), "echo hi".to_string()),
            (
                "assistant".into(),
                r#"call:call_m1:a__echo:{"text":"hi"}"#.into()
            ),
            ("tool".into(), "result:call_m1:echo: hi".into()),
        ]
    );
    assert!(
        log.text().contains("MCP tools ") && log.text().contains(" ms (1 call)"),
        "{}",
        log.text()
    );
}

/// One `realtime-tool` row per call, under the session's key, as a tool
/// row: no tokens, and nothing added to the token stats or the request
/// counters, which count the model calls alone (§2.4, §4).
#[tokio::test]
async fn each_call_is_a_realtime_tool_row_and_the_token_stats_count_only_the_model() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[
            (0, "call_r1", "a__echo", r#"{"text":"one"}"#),
            (1, "call_r2", "a__echo", r#"{"text":"two"}"#),
        ],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, true, Some(KeyPolicy::default()), |_| {}).await;
    let _stub = alpha(&state, echo_stub().await).await;
    let before = state.telemetry.stats();
    let mut ws = tools_session(&addr, Some(KEY), hosted_a(), 1).await;
    ask(&mut ws, "twice").await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    let tools = rows(&state, "realtime-tool").await;
    assert_eq!(tools.len(), 2, "one row per call");
    for r in &tools {
        assert_eq!(r.mcp_tool.as_deref(), Some("a__echo"));
        assert_eq!(r.client_key.as_deref(), Some("voice"));
        assert!(r.key_id.is_some());
        assert_eq!(r.class.as_deref(), Some("tool"));
        assert_eq!(r.status, 200);
        assert!(r.prompt_tokens.is_none() && r.completion_tokens.is_none());
    }
    let model = rows(&state, "realtime").await;
    assert_eq!(model.len(), 1);
    let after = state.telemetry.stats();
    assert_eq!(after.total_requests - before.total_requests, 1);
    assert_eq!(after.prompt_tokens - before.prompt_tokens, 6);
    assert_eq!(after.active_requests, 0);
    assert!(!lmgw_core::telemetry::counts_in_token_stats(
        "realtime-tool"
    ));
}

/// A tool that answers `isError` and a server that fails the call outright
/// both end the call `.failed` with a `tool_execution_error` in their words
/// — and the model reads both on the next response (§2.4).
#[tokio::test]
async fn a_tool_error_and_a_failed_call_end_failed_and_the_model_reads_them() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[
            (0, "call_e1", "a__fail", r#"{"room":"attic"}"#),
            (1, "call_e2", "a__boom", "{}"),
        ],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let fails = answer(|name, _| async move {
        match name.as_str() {
            "fail" => json!({"content": [{"type": "text", "text": "no such room"}],
                             "isError": true}),
            _ => Value::Null,
        }
    });
    let tools = json!([{"name": "fail", "inputSchema": {"type": "object"}},
                       {"name": "boom", "inputSchema": {"type": "object"}}]);
    let _stub = alpha(&state, answering(tools, false, fails).await).await;
    let mut ws = tools_session(&addr, None, hosted_a(), 1).await;
    ask(&mut ws, "try").await;
    let events = events_until(&mut ws, "response.done").await;
    assert_every_mcp_call(&events);
    let items: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .map(|e| &e["item"])
        .collect();
    assert_eq!(items.len(), 2, "{events:#?}");
    for item in &items {
        let id = item["id"].as_str().unwrap();
        at(&events, "response.mcp_call.failed", id);
        assert_eq!(item["output"], Value::Null, "{item}");
        assert_eq!(item["error"]["type"], "tool_execution_error", "{item}");
    }
    assert_eq!(items[0]["error"]["message"], "no such room");
    let boom = items[1]["error"]["message"].as_str().unwrap().to_string();
    assert!(!boom.is_empty());
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.done").await;
    let sent = shape(&fake.seen.chat(1));
    assert_eq!(
        sent[2],
        ("tool".into(), "result:call_e1:no such room".into())
    );
    assert_eq!(sent[3], ("tool".into(), format!("result:call_e2:{boom}")));
}

/// §3: the function call completes at the model's `Stop` as today, while
/// the server call still runs; one `response.done` follows both, and the
/// client's one follow-up renders both calls in one assistant turn with
/// their results.
#[tokio::test]
async fn a_function_and_an_mcp_call_in_one_turn_end_in_one_response_done() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[
            (0, "call_f", "f", "{}"),
            (1, "call_m", "a__echo", r#"{"text":"x"}"#),
        ],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let gate = Arc::new(Notify::new());
    let held = gate.clone();
    let slow = answer(move |_, args| {
        let held = held.clone();
        async move {
            held.notified().await;
            let text = args["text"].as_str().unwrap_or_default().to_string();
            json!({"content": [{"type": "text", "text": format!("echo: {text}")}]})
        }
    });
    let _stub = alpha(&state, answering(echo_tools(), false, slow).await).await;
    let tools = json!([{"type": "function", "name": "f", "parameters": {"type": "object"}},
                       {"type": "mcp", "server_label": "a"}]);
    let mut ws = tools_session(&addr, None, tools, 1).await;
    ask(&mut ws, "both").await;

    let mut events = events_until(&mut ws, "response.mcp_call.in_progress").await;
    let fc = events
        .iter()
        .find(|e| e["type"] == "response.output_item.done")
        .map(|e| e["item"].clone())
        .expect("the function call completed at the Stop");
    assert_eq!(
        (&fc["type"], &fc["status"]),
        (&json!("function_call"), &json!("completed"))
    );
    // The client runs its function and sends one follow-up, held by the SDK
    // until the response is done — or at once, which queues it.
    send(
        &mut ws,
        json!({"type": "conversation.item.create", "item": {"type": "function_call_output",
               "call_id": "call_f", "output": "f says hello"}}),
    )
    .await;
    send(&mut ws, json!({"type": "response.create"})).await;
    gate.notify_one();
    events.extend(events_until(&mut ws, "response.done").await);
    let done = events.last().unwrap()["response"].clone();
    assert_eq!(done["status"], "completed");
    let output_types: Vec<&str> = done["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["type"].as_str().unwrap())
        .collect();
    assert_eq!(output_types, ["function_call", "mcp_call"]);
    assert_eq!(done["output"][1]["output"], "echo: x");
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "response.done")
            .count(),
        1
    );
    events.extend(events_until(&mut ws, "response.done").await);
    assert!(!events.iter().any(|e| e["type"] == "error"), "{events:#?}");
    assert_eq!(
        shape(&fake.seen.chat(1)),
        [
            ("user".to_string(), "both".to_string()),
            (
                "assistant".into(),
                r#"call:call_f:f:{} | call:call_m:a__echo:{"text":"x"}"#.into()
            ),
            ("tool".into(), "result:call_f:f says hello".into()),
            ("tool".into(), "result:call_m:echo: x".into()),
        ]
    );
}

/// With `parallel_tool_calls: false` the calls run one at a time, in the
/// model's order; by default they start together (§2.4). Whether one
/// server then answers them at once is its transport's business: rmcp's
/// HTTP client sends a server's requests one after another when it answers
/// with JSON, as this stub does — so "together" is judged by the starts.
#[tokio::test]
async fn without_parallel_tool_calls_the_calls_run_in_model_order() {
    let fake = chat_fake().await;
    let two = || {
        calls(
            &[
                (0, "c_one", "a__echo", r#"{"text":"one"}"#),
                (1, "c_two", "a__echo", r#"{"text":"two"}"#),
            ],
            "tool_calls",
        )
    };
    fake.push(two());
    fake.push(two());
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    type Spans = Arc<Mutex<Vec<(String, Instant, Instant)>>>;
    let spans: Spans = Arc::default();
    let seen = spans.clone();
    let timed = answer(move |_, args| {
        let seen = seen.clone();
        async move {
            let text = args["text"].as_str().unwrap_or_default().to_string();
            let from = Instant::now();
            tokio::time::sleep(Duration::from_millis(300)).await;
            seen.lock()
                .unwrap()
                .push((text.clone(), from, Instant::now()));
            json!({"content": [{"type": "text", "text": text}]})
        }
    });
    let _stub = alpha(&state, answering(echo_tools(), false, timed).await).await;
    let mut ws = tools_session(&addr, None, hosted_a(), 1).await;
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "parallel_tool_calls": false}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    let progress = |events: &[Value]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e["type"].as_str() {
                Some("response.mcp_call.in_progress") => Some("start".to_string()),
                Some("response.mcp_call.completed") => Some("end".to_string()),
                _ => None,
            })
            .collect()
    };
    ask(&mut ws, "in order").await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(progress(&events), ["start", "end", "start", "end"]);
    {
        let s = spans.lock().unwrap();
        assert_eq!(
            s.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert!(s[1].1 >= s[0].2, "the second started after the first ended");
    }
    assert_eq!(fake.seen.chat(0)["parallel_tool_calls"], false);

    spans.lock().unwrap().clear();
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "parallel_tool_calls": true}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    ask(&mut ws, "together").await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(progress(&events), ["start", "start", "end", "end"]);
    assert_eq!(spans.lock().unwrap().len(), 2);
}

/// §2.3: a response cut off for `Length`, arguments that are not a JSON
/// object, and a stream that fails after its `Stop` run nothing. The calls
/// fail at once — never made, or naming the parse error — and the server
/// sees no call and writes no row.
#[tokio::test]
async fn a_cut_off_response_bad_arguments_and_a_failure_after_stop_run_nothing() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[(0, "c_len", "a__echo", r#"{"text":"x"}"#)],
        "length",
    ));
    fake.push(calls(
        &[(0, "c_bad", "a__echo", r#"{"text": "#)],
        "tool_calls",
    ));
    fake.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("c_fail"),
            name: "a__echo",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"text":"x"}"#,
        },
        Step::Finish("tool_calls"),
        Step::Fail,
    ]));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let stub = alpha(&state, echo_stub().await).await;
    let mut ws = tools_session(&addr, None, hosted_a(), 1).await;

    let failed = |events: &[Value]| -> Value {
        assert!(
            !events
                .iter()
                .any(|e| e["type"] == "response.mcp_call.in_progress"),
            "{events:#?}"
        );
        let item = events
            .iter()
            .find(|e| e["type"] == "response.output_item.done")
            .map(|e| e["item"].clone())
            .unwrap();
        at(
            events,
            "response.mcp_call.failed",
            item["id"].as_str().unwrap(),
        );
        assert_mcp_call(&item);
        assert_eq!(item["output"], Value::Null);
        assert_eq!(item["error"]["type"], "tool_execution_error");
        item
    };
    const UNMADE: &str = "not run: the turn ended before this call was made";

    ask(&mut ws, "cut off").await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(failed(&events)["error"]["message"], UNMADE);
    let done = &events.last().unwrap()["response"];
    assert_eq!(
        (&done["status"], &done["status_details"]["reason"]),
        (&json!("incomplete"), &json!("max_output_tokens"))
    );

    ask(&mut ws, "bad arguments").await;
    let events = events_until(&mut ws, "response.done").await;
    let message = failed(&events)["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("not valid JSON"), "{message}");
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    ask(&mut ws, "fails after").await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(failed(&events)["error"]["message"], UNMADE);
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    assert!(stub.calls().is_empty(), "{:?}", stub.calls());
    assert!(rows(&state, "realtime-tool").await.is_empty());
}
