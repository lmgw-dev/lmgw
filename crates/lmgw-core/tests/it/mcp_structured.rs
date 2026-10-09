//! A tool result's `structuredContent` stays out of the model's context when
//! its `content` says something (client-apps design §7.5; the owner,
//! 2026-10-09: MCP's `content` is the model's, `structuredContent` the host's
//! and its views'); a result whose `content` is empty is given to the model
//! as its structured content, as before. Per path that makes a tool result
//! model input — a Chat turn, `/v1/responses`, a realtime session (an MCP
//! task's result: `mcp::tasks`' unit tests) — while the Chat's frames carry
//! both to the client, the content blocks as the server sent them.

use serde_json::{json, Value};

use crate::device_chat::sse;
use crate::mcp_resources::{server, weather};
use crate::realtime_chat_thread::{world, World};
use crate::support::mcp_apps_stub::Apps;
use crate::support::mcp_stub::{answer, answering, register};
use crate::support::realtime_fakes::{chat_fake, events_until, gateway, send, user_text, Turn};
use crate::support::realtime_mcp::{calls, tools_session};

/// What the weather server's `show` answers with besides its content.
const STRUCTURED: &str = "temp_c";

/// A server `data` (prefix `db`) whose `rows` answers with structured
/// content and no content blocks.
fn data() -> Apps {
    Apps {
        tools: json!([{"name": "rows", "inputSchema": {"type": "object"}}]),
        call_result: json!({"content": [], "structuredContent": {"rows_found": 3}}),
        resources: json!([]),
        templates: json!([]),
    }
}

/// The tool messages of the `n`th model request, as one string each.
fn tool_messages(w: &World, n: usize) -> Vec<String> {
    let req = w.chat.seen.chat(n);
    req["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("{req}"))
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].to_string())
        .collect()
}

/// A Chat turn: the model reads `show`'s text, not its structured content,
/// and `rows`' structured content, which is all it said; the result frames
/// carry both, the content blocks as sent (resource URIs namespaced).
#[tokio::test]
async fn a_chat_turn_gives_the_model_content_and_the_frame_both() {
    let w = world(|_| {}).await;
    let _wx = server(&w, "wx", "wx", weather()).await;
    let _db = server(&w, "data", "db", data()).await;
    let owner = w.gw.client();
    let tid = w.thread("chatty", json!({})).await;
    w.set(
        tid,
        json!({"mcp_tools": [{"server_label": "wx"}, {"server_label": "db"}]}),
    )
    .await;
    w.chat.push(calls(
        &[(0, "c1", "wx__show", "{}"), (1, "c2", "db__rows", "{}")],
        "tool_calls",
    ));
    w.chat.push(Turn::text(&["done"]));
    let (s, frames) = sse(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "go"}),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");

    let tools = tool_messages(&w, 1);
    assert_eq!(tools.len(), 2, "{tools:?}");
    assert!(tools[0].contains("sunny"), "{}", tools[0]);
    assert!(!tools[0].contains(STRUCTURED), "{}", tools[0]);
    assert!(tools[1].contains("rows_found"), "{}", tools[1]);

    let result = |name: &str| -> Value {
        frames
            .iter()
            .find(|(e, d)| e == "tool" && d["event"] == "result" && d["name"] == name)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| panic!("no result for {name}: {frames:?}"))
    };
    let show = result("wx__show");
    assert_eq!(show["structured_content"][STRUCTURED], 21, "{show}");
    assert!(!show["output"].to_string().contains(STRUCTURED), "{show}");
    assert_eq!(show["content"][0], json!({"type": "text", "text": "sunny"}));
    assert_eq!(show["content"][1]["type"], "resource_link", "{show}");
    assert_eq!(show["content"][1]["uri"], "ui://wx__weather/card", "{show}");
    assert_eq!(show["call_id"], "c1", "{show}");
    let rows = result("db__rows");
    assert_eq!(rows["structured_content"], json!({"rows_found": 3}));
    assert_eq!(rows["content"], json!([]), "{rows}");
}

/// `/v1/responses`: the model's next request and the `mcp_call` items hold
/// `show`'s text alone, and `rows`' structured content.
#[tokio::test]
async fn responses_give_the_model_content() {
    let w = world(|_| {}).await;
    let _wx = server(&w, "wx", "wx", weather()).await;
    let _db = server(&w, "data", "db", data()).await;
    w.chat.push(calls(
        &[(0, "c1", "wx__show", "{}"), (1, "c2", "db__rows", "{}")],
        "tool_calls",
    ));
    w.chat.push(Turn::text(&["done"]));
    // Streamed: the chat fake streams its tool calls.
    let body = json!({"model": "chatty", "input": "go", "stream": true,
                      "tools": [{"type": "mcp", "server_label": "wx"},
                                {"type": "mcp", "server_label": "db"}]});
    let (s, frames) = sse(&w, &w.gw.client(), "/v1/responses", body).await;
    assert_eq!(s, 200, "{frames:?}");
    let v = frames
        .iter()
        .find(|(e, _)| e == "response.completed")
        .map(|(_, d)| d["response"].clone())
        .unwrap_or_else(|| panic!("no response.completed: {frames:?}"));
    assert_eq!(w.chat.seen.chat_count(), 2, "{v}");
    let tools = tool_messages(&w, 1);
    assert!(
        tools[0].contains("sunny") && !tools[0].contains(STRUCTURED),
        "{tools:?}"
    );
    assert!(tools[1].contains("rows_found"), "{tools:?}");
    let call = |name: &str| -> Value {
        v["output"]
            .as_array()
            .unwrap_or_else(|| panic!("{v}"))
            .iter()
            .find(|i| i["type"] == "mcp_call" && i["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no mcp_call {name}: {v}"))
    };
    assert!(
        !call("wx__show")["output"].to_string().contains(STRUCTURED),
        "{v}"
    );
    assert!(
        call("db__rows")["output"]
            .to_string()
            .contains("rows_found"),
        "{v}"
    );
}

/// A realtime session: the `mcp_call`'s output and the model's next
/// request hold the content alone.
#[tokio::test]
async fn a_realtime_session_gives_the_model_content() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[(0, "call_1", "wx__show", r#"{"text":"x"}"#)],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let stub = answering(
        json!([{"name": "show", "inputSchema": {"type": "object"}}]),
        false,
        answer(|_, _| async {
            json!({"content": [{"type": "text", "text": "sunny"}],
                   "structuredContent": {"temp_c": 21}})
        }),
    )
    .await;
    register(&state, "weather", "wx", &stub.url, true, None).await;
    let tools = json!([{"type": "mcp", "server_label": "wx"}]);
    let mut ws = tools_session(&addr, None, tools, 1).await;

    send(&mut ws, user_text("weather?")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    let done = &events.last().unwrap()["response"];
    let call = done["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_call")
        .cloned()
        .unwrap_or_else(|| panic!("no mcp_call in {done}"));
    assert_eq!(call["output"], "sunny", "{call}");

    fake.push(Turn::text(&["it is sunny"]));
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.done").await;
    let req = fake.seen.chat(1);
    let tool = req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .cloned()
        .unwrap_or_else(|| panic!("no tool message in {req}"));
    assert!(tool.to_string().contains("sunny"), "{tool}");
    assert!(!tool.to_string().contains(STRUCTURED), "{tool}");
}
