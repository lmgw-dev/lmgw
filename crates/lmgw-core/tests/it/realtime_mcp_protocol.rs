//! Server-side MCP tools on `/v1/realtime`, the protocol half
//! (realtime-server-tools design §1.1, §1.3, §2.2, §2.6; WP1): the `mcp`
//! session tool through `session.update` — the shared `allowed_tools` /
//! `require_approval` parser, this route's own refusals, label-only reuse and
//! the redacted echo — `tool_choice` naming a label, the `mcp_call` /
//! `mcp_list_tools` items with their always-written fields, the MCP events,
//! and which `mcp_*` items a client may create.

use lmgw_core::config::RealtimeSettings;
use lmgw_core::realtime::merge::{apply_update, initial_session};
use lmgw_core::realtime::protocol::{
    ClientEvent, ErrorObject, Item, McpCallError, McpCallItem, McpErrorKind, McpListToolsItem,
    McpListedTool, ResponseObject, ResponseStatus, ServerEvent, ServerFrame, Session,
};
use serde_json::{json, Map, Value};

use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, send, text_session, Ws,
};

fn settings() -> RealtimeSettings {
    RealtimeSettings::default()
}

fn fresh() -> Session {
    initial_session(&settings(), "sess_mcp".into(), Some("chatty".into()))
}

fn update(current: &Session, session: Value) -> Result<Session, ErrorObject> {
    let mut patch: Map<String, Value> = session.as_object().unwrap().clone();
    patch.insert("type".into(), json!("realtime"));
    apply_update(current, &patch, &settings())
}

fn tools_of(s: &Session) -> Value {
    serde_json::to_value(s).unwrap()["tools"].clone()
}

// ---------------------------------------------------------------------------
// The session tool
// ---------------------------------------------------------------------------

/// `@openai/agents` sends `allowed_tools` as `{tool_names}`; the array form
/// is OpenAI's other spelling. Both are taken and echoed as written, beside a
/// function tool.
#[test]
fn an_mcp_tool_takes_both_allowed_tools_forms_and_echoes_as_written() {
    let tools = json!([
        {"type": "mcp", "server_label": "docs", "allowed_tools": ["query"],
         "require_approval": "never"},
        {"type": "mcp", "server_label": "kb", "server_url": "https://example.invalid/mcp",
         "allowed_tools": {"tool_names": ["kb__search"]}, "server_description": "notes"},
        {"type": "function", "name": "f"},
    ]);
    let next = update(&fresh(), json!({"tools": tools})).unwrap();
    assert_eq!(tools_of(&next), tools);
    assert!(next.tools.as_ref().unwrap()[0].as_mcp().is_some());
    assert!(next.tools.as_ref().unwrap()[2].as_mcp().is_none());
}

/// `authorization` and every header value are replaced as the tool is
/// parsed, so neither the echo nor the session itself holds them; header
/// names stay.
#[test]
fn secrets_are_redacted_in_the_session_and_its_echo() {
    let next = update(
        &fresh(),
        json!({"tools": [{"type": "mcp", "server_label": "docs",
                          "authorization": "Bearer sk-very-secret",
                          "headers": {"X-Api-Key": "hdr-very-secret",
                                      "X-Trace": "trace-very-secret"}}]}),
    )
    .unwrap();
    let echoed = serde_json::to_value(ServerFrame {
        event_id: "event_1".into(),
        event: ServerEvent::SessionUpdated {
            session: Box::new(next.clone()),
        },
    })
    .unwrap();
    let tool = &echoed["session"]["tools"][0];
    assert_eq!(tool["authorization"], "[redacted]");
    assert_eq!(
        tool["headers"],
        json!({"X-Api-Key": "[redacted]", "X-Trace": "[redacted]"})
    );
    for (what, text) in [
        ("echo", echoed.to_string()),
        ("session", format!("{next:?}")),
    ] {
        assert!(!text.contains("very-secret"), "{what}: {text}");
    }
    // A client sending back the session it was given changes nothing.
    let again = update(&next, echoed["session"].clone()).unwrap();
    assert_eq!(tools_of(&again), tools_of(&next));
}

#[test]
fn read_only_is_refused_naming_the_entry() {
    for (tool, param) in [
        (
            json!({"type": "mcp", "server_label": "docs", "allowed_tools": {"read_only": true}}),
            "session.tools[1].allowed_tools",
        ),
        (
            json!({"type": "mcp", "server_label": "docs",
                   "allowed_tools": {"tool_names": ["query"], "read_only": false}}),
            "session.tools[1].allowed_tools",
        ),
        (
            json!({"type": "mcp", "server_label": "docs",
                   "require_approval": {"never": {"read_only": true}}}),
            "session.tools[1].require_approval",
        ),
    ] {
        let e = update(
            &fresh(),
            json!({"tools": [{"type": "function", "name": "f"}, tool]}),
        )
        .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("invalid_value"), "{tool}");
        assert_eq!(e.param.as_deref(), Some(param), "{tool}");
        assert!(
            e.message
                .contains("lmgw does not read MCP tool annotations, so `read_only` cannot be honoured; list the tool names"),
            "{}",
            e.message
        );
    }
}

/// A tool name that is not a string is refused by its index, as on
/// `/v1/responses`: dropped, it would leave another filter than the one
/// written.
#[test]
fn a_tool_name_that_is_not_a_string_is_refused_by_its_index() {
    let e = update(
        &fresh(),
        json!({"tools": [{"type": "mcp", "server_label": "docs",
                          "allowed_tools": {"tool_names": ["query", false]}}]}),
    )
    .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("invalid_value"));
    assert_eq!(e.param.as_deref(), Some("session.tools[0].allowed_tools"));
    assert!(
        e.message
            .ends_with("on mcp server 'docs': entry 1 is a boolean, not a tool name"),
        "{}",
        e.message
    );
}

/// No approvals on this route yet (decision 4): what could gate a tool is
/// refused, what provably gates none is taken.
#[test]
fn a_gating_require_approval_is_refused_and_one_that_gates_nothing_is_taken() {
    for rule in [
        json!("always"),
        json!({"never": {"tool_names": ["query"]}}),
        json!({"always": {"tool_names": ["query"]}}),
        json!({"always": {"tool_names": ["query", "resolve"]},
               "never": {"tool_names": ["query"]}}),
    ] {
        let e = update(
            &fresh(),
            json!({"tools": [{"type": "mcp", "server_label": "docs", "require_approval": rule}]}),
        )
        .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("invalid_value"), "{rule}");
        assert_eq!(
            e.param.as_deref(),
            Some("session.tools[0].require_approval"),
            "{rule}"
        );
        assert!(
            e.message.contains("approvals are not built"),
            "{}",
            e.message
        );
    }
    for rule in [
        json!("never"),
        json!(null),
        json!({"never": {"tool_names": []}}),
        json!({"always": {"tool_names": ["query"]}, "never": {"tool_names": ["query"]}}),
    ] {
        update(
            &fresh(),
            json!({"tools": [{"type": "mcp", "server_label": "docs", "require_approval": rule}]}),
        )
        .unwrap_or_else(|e| panic!("{rule}: {}", e.message));
    }
    // An unknown spelling is the shared parser's refusal.
    let e = update(
        &fresh(),
        json!({"tools": [{"type": "mcp", "server_label": "docs", "require_approval": "maybe"}]}),
    )
    .unwrap_err();
    assert_eq!(
        e.param.as_deref(),
        Some("session.tools[0].require_approval")
    );
    assert!(e.message.contains("maybe"), "{}", e.message);
}

#[test]
fn a_label_twice_in_one_array_is_refused_and_changes_nothing() {
    let current = update(
        &fresh(),
        json!({"tools": [{"type": "mcp", "server_label": "kb"}]}),
    )
    .unwrap();
    let e = update(
        &current,
        json!({"tools": [
            {"type": "mcp", "server_label": "docs"},
            {"type": "function", "name": "f"},
            {"type": "mcp", "server_label": "docs", "allowed_tools": ["query"]},
        ]}),
    )
    .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("invalid_value"));
    assert_eq!(e.param.as_deref(), Some("session.tools[2].server_label"));
    assert!(e.message.contains("'docs'"), "{}", e.message);
    // `apply_update` answers a new session or an error; the one in effect
    // is still the one it was.
    assert_eq!(
        tools_of(&current),
        json!([{"type": "mcp", "server_label": "kb"}])
    );
}

/// `{type: "mcp", server_label}` alone takes the earlier definition of that
/// label, filters and all; an entry with anything more is a definition of
/// its own, and a label never defined stays as written.
#[test]
fn a_label_alone_reuses_the_session_s_definition() {
    let defined = json!({"type": "mcp", "server_label": "docs",
                         "allowed_tools": {"tool_names": ["query"]},
                         "require_approval": "never", "authorization": "secret"});
    let first = update(&fresh(), json!({"tools": [defined]})).unwrap();
    let kept = tools_of(&first)[0].clone();
    assert_eq!(kept["authorization"], "[redacted]");

    let second = update(
        &first,
        json!({"tools": [
            {"type": "function", "name": "f"},
            {"type": "mcp", "server_label": "docs"},
            {"type": "mcp", "server_label": "kb"},
        ]}),
    )
    .unwrap();
    let tools = tools_of(&second);
    assert_eq!(tools[1], kept);
    assert_eq!(tools[2], json!({"type": "mcp", "server_label": "kb"}));

    let redefined = update(
        &second,
        json!({"tools": [{"type": "mcp", "server_label": "docs", "allowed_tools": ["resolve"]}]}),
    )
    .unwrap();
    assert_eq!(
        tools_of(&redefined),
        json!([{"type": "mcp", "server_label": "docs", "allowed_tools": ["resolve"]}])
    );
}

#[test]
fn tool_choice_names_an_mcp_label_with_or_without_a_tool() {
    for choice in [
        json!({"type": "mcp", "server_label": "docs"}),
        json!({"type": "mcp", "server_label": "docs", "name": "query"}),
    ] {
        let next = update(&fresh(), json!({"tool_choice": choice})).unwrap();
        assert_eq!(serde_json::to_value(&next).unwrap()["tool_choice"], choice);
    }
    // A label is required.
    assert!(update(&fresh(), json!({"tool_choice": {"type": "mcp"}})).is_err());
}

/// A `response.create`'s own tools take `mcp` entries, by the same rules.
#[test]
fn a_response_create_carries_mcp_tools_and_choice() {
    let frame = json!({"type": "response.create", "response": {
        "tools": [{"type": "mcp", "server_label": "docs", "allowed_tools": ["query"]}],
        "tool_choice": {"type": "mcp", "server_label": "docs", "name": "query"}}});
    let ev: ClientEvent = serde_json::from_value(frame.clone()).unwrap();
    assert_eq!(serde_json::to_value(&ev).unwrap(), frame);
}

// ---------------------------------------------------------------------------
// Items and events
// ---------------------------------------------------------------------------

fn bare_call() -> Item {
    Item::McpCall(Box::new(McpCallItem {
        id: Some("item_7".into()),
        server_label: "docs".into(),
        name: "query".into(),
        arguments: String::new(),
        approval_request_id: None,
        output: None,
        error: None,
    }))
}

/// `@openai/agents` throws on an `mcp_call` without `output`, inside a
/// listener with no try/catch (§1.3): every field is written, `null` where
/// empty, on every event that carries the item.
#[test]
fn an_mcp_call_always_writes_its_fields() {
    let expected = json!({"type": "mcp_call", "id": "item_7", "server_label": "docs",
                          "name": "query", "arguments": "", "approval_request_id": null,
                          "output": null, "error": null});
    assert_eq!(serde_json::to_value(bare_call()).unwrap(), expected);

    let response = ResponseObject {
        id: "resp_1".into(),
        object: "realtime.response".into(),
        status: ResponseStatus::Completed,
        status_details: None,
        output: vec![bare_call()],
        conversation_id: None,
        output_modalities: vec![],
        max_output_tokens: lmgw_core::realtime::protocol::MaxOutputTokens::Count(1),
        audio: None,
        usage: None,
        metadata: None,
    };
    for (event, at) in [
        (
            ServerEvent::ItemAdded {
                previous_item_id: None,
                item: bare_call(),
            },
            "/item",
        ),
        (
            ServerEvent::ItemDone {
                previous_item_id: None,
                item: bare_call(),
            },
            "/item",
        ),
        (ServerEvent::ItemRetrieved { item: bare_call() }, "/item"),
        (
            ServerEvent::OutputItemAdded {
                response_id: "resp_1".into(),
                output_index: 0,
                item: bare_call(),
            },
            "/item",
        ),
        (
            ServerEvent::OutputItemDone {
                response_id: "resp_1".into(),
                output_index: 0,
                item: bare_call(),
            },
            "/item",
        ),
        (
            ServerEvent::ResponseDone {
                response: Box::new(response),
            },
            "/response/output/0",
        ),
    ] {
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v.pointer(at), Some(&expected), "{v}");
    }

    // A finished call: one of the two set, the other still written.
    let failed = Item::McpCall(Box::new(McpCallItem {
        error: Some(McpCallError {
            kind: McpErrorKind::ToolExecutionError,
            code: None,
            message: "boom".into(),
        }),
        ..match bare_call() {
            Item::McpCall(c) => *c,
            _ => unreachable!(),
        }
    }));
    let v = serde_json::to_value(failed).unwrap();
    assert_eq!(v["output"], Value::Null);
    assert_eq!(
        v["error"],
        json!({"type": "tool_execution_error", "message": "boom"})
    );
}

/// `@openai/agents` drops the whole event over a `null` description.
#[test]
fn an_mcp_list_tools_item_writes_a_string_description_and_null_annotations() {
    let item = Item::McpListTools(McpListToolsItem {
        id: Some("item_8".into()),
        server_label: "docs".into(),
        tools: vec![McpListedTool {
            name: "query".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            annotations: None,
        }],
    });
    assert_eq!(
        serde_json::to_value(item).unwrap(),
        json!({"type": "mcp_list_tools", "id": "item_8", "server_label": "docs",
               "tools": [{"name": "query", "description": "", "input_schema": {"type": "object"},
                          "annotations": null}]})
    );
}

/// Each MCP event with exactly the ids the GA reference gives it:
/// `response.mcp_call.*` carry no `response_id`, the argument events do.
#[test]
fn the_mcp_events_carry_exactly_their_ids() {
    let fixtures = [
        json!({"type": "mcp_list_tools.in_progress", "item_id": "item_8"}),
        json!({"type": "mcp_list_tools.completed", "item_id": "item_8"}),
        json!({"type": "mcp_list_tools.failed", "item_id": "item_8"}),
        json!({"type": "response.mcp_call_arguments.delta", "response_id": "resp_1",
               "item_id": "item_7", "output_index": 1, "delta": "{\"q\""}),
        json!({"type": "response.mcp_call_arguments.done", "response_id": "resp_1",
               "item_id": "item_7", "output_index": 1, "arguments": "{\"q\":1}"}),
        json!({"type": "response.mcp_call.in_progress", "item_id": "item_7", "output_index": 1}),
        json!({"type": "response.mcp_call.completed", "item_id": "item_7", "output_index": 1}),
        json!({"type": "response.mcp_call.failed", "item_id": "item_7", "output_index": 1}),
    ];
    for mut fixture in fixtures {
        fixture["event_id"] = json!("event_9");
        let frame: ServerFrame =
            serde_json::from_value(fixture.clone()).unwrap_or_else(|e| panic!("{fixture}: {e}"));
        assert_eq!(serde_json::to_value(&frame).unwrap(), fixture);
    }
}

// ---------------------------------------------------------------------------
// Over a socket
// ---------------------------------------------------------------------------

async fn refused(ws: &mut Ws, event: Value) -> Value {
    send(ws, event).await;
    let e = next_event(ws).await;
    assert_eq!(e["type"], "error", "{e}");
    e
}

/// `session.updated` carries the redacted tool; a refused update is one
/// `error` naming the entry, and the session is as it was.
#[tokio::test]
async fn session_updated_echoes_the_tool_redacted_and_a_refusal_changes_nothing() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;

    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime", "tools": [
            {"type": "mcp", "server_label": "docs", "authorization": "sk-socket-secret",
             "allowed_tools": {"tool_names": ["query"]}, "require_approval": "never"}]}}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    let tool = &updated["session"]["tools"][0];
    assert_eq!(tool["authorization"], "[redacted]");
    assert_eq!(tool["allowed_tools"], json!({"tool_names": ["query"]}));
    assert!(!updated.to_string().contains("sk-socket-secret"));
    // The label is listed (realtime_mcp_listing.rs).
    let listing = events_until(&mut ws, "conversation.item.done").await;
    assert!(!format!("{listing:?}").contains("sk-socket-secret"));

    let e = refused(
        &mut ws,
        json!({"type": "session.update", "event_id": "u2", "session": {"type": "realtime",
               "tools": [{"type": "mcp", "server_label": "docs", "require_approval": "always"}]}}),
    )
    .await;
    assert_eq!(e["error"]["code"], "invalid_value");
    assert_eq!(e["error"]["param"], "session.tools[0].require_approval");
    assert_eq!(e["error"]["event_id"], "u2");

    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime"}}),
    )
    .await;
    let after = next_event(&mut ws).await;
    assert_eq!(after["session"]["tools"], updated["session"]["tools"]);
}

/// A client's `mcp_call` is history and is taken whole; the listing and the
/// approval items are refused naming the type, and the session goes on.
#[tokio::test]
async fn a_client_may_replay_an_mcp_call_but_not_create_the_other_mcp_items() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;

    send(
        &mut ws,
        json!({"type": "conversation.item.create", "item": {
            "type": "mcp_call", "server_label": "docs", "name": "query",
            "arguments": "{\"q\":\"axum\"}", "output": "three hits"}}),
    )
    .await;
    let added = next_event(&mut ws).await;
    assert_eq!(added["type"], "conversation.item.added", "{added}");
    let item = &added["item"];
    assert!(item["id"].as_str().unwrap().starts_with("item_"), "{item}");
    assert_eq!(item["output"], "three hits");
    for key in ["error", "approval_request_id"] {
        assert_eq!(item.get(key), Some(&Value::Null), "{key}: {item}");
    }
    let done = next_event(&mut ws).await;
    assert_eq!(done["type"], "conversation.item.done");
    assert_eq!(done["item"], added["item"]);

    for (item, kind) in [
        (
            json!({"type": "mcp_list_tools", "server_label": "docs", "tools": []}),
            "mcp_list_tools",
        ),
        // Shapes that would not parse as an item at all.
        (json!({"type": "mcp_list_tools"}), "mcp_list_tools"),
        (
            json!({"type": "mcp_approval_request", "server_label": "docs", "name": "query",
                   "arguments": "{}"}),
            "mcp_approval_request",
        ),
        (
            json!({"type": "mcp_approval_response", "approval_request_id": "x",
                   "approve": true}),
            "mcp_approval_response",
        ),
    ] {
        let e = refused(
            &mut ws,
            json!({"type": "conversation.item.create", "event_id": "c1", "item": item}),
        )
        .await;
        assert_eq!(e["error"]["code"], "invalid_value", "{e}");
        assert_eq!(e["error"]["param"], "item.type", "{e}");
        assert_eq!(e["error"]["event_id"], "c1", "{e}");
        let message = e["error"]["message"].as_str().unwrap();
        assert!(message.contains(kind), "{message}");
    }

    // Still answering.
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": item["id"]}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "conversation.item.retrieved"
    );
}

/// A session's function tools still reach the model beside an `mcp` entry,
/// first; a `tool_choice` naming a label the response's tools do not name
/// is an `error`, and nothing reaches the model.
#[tokio::test]
async fn an_mcp_entry_beside_functions_and_an_unlisted_choice() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime", "tools": [
            {"type": "mcp", "server_label": "docs"},
            {"type": "function", "name": "f", "parameters": {"type": "object"}}]}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    events_until(&mut ws, "conversation.item.done").await;

    let e = refused(
        &mut ws,
        json!({"type": "response.create", "response": {
            "tool_choice": {"type": "mcp", "server_label": "kb"}}}),
    )
    .await;
    assert_eq!(e["error"]["param"], "response.tool_choice", "{e}");
    assert!(
        e["error"]["message"].as_str().unwrap().contains("'kb'"),
        "{e}"
    );
    assert_eq!(fake.seen.chat_count(), 0);

    send(
        &mut ws,
        json!({"type": "conversation.item.create", "item": {"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "hi"}]}}),
    )
    .await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:?}"
    );
    let body = fake.seen.chat(0);
    assert_eq!(body["tools"][0]["function"]["name"], "f", "{body}");
}
