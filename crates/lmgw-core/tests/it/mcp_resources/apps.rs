//! The MCP Apps metadata: one namespaced URI everywhere, `visibility` in
//! the runs that offer tools to a model, the Chat `tool` frame.

use serde_json::{json, Value};

use super::{apps_session, connect_all, listed, rpc, server, weather, CARD};
use crate::device_chat::sse;
use crate::mcp_host::mcp_rpc;
use crate::realtime_chat_thread::world;
use crate::support::mcp_apps_stub::{page, Apps};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_mcp::calls;

const NAMESPACED: &str = "ui://wx__weather/card";

/// The URI a tool links to in `tools/list`, the URIs in its result, the
/// listed resource and the read are one and the same.
#[tokio::test]
async fn one_namespaced_uri_from_the_tool_list_to_a_result_and_a_read() {
    let w = world(|_| {}).await;
    let wx = server(&w, "wx", "wx", weather()).await;
    connect_all(&w).await;
    let owner = w.gw.client();

    let v = rpc(&w, &owner, "tools/list", json!({})).await;
    let show = v["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .find(|t| t["name"] == "wx__show")
        .cloned()
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(show["_meta"]["ui"]["resourceUri"], NAMESPACED, "{show}");
    assert_eq!(show["_meta"]["ui/resourceUri"], NAMESPACED, "{show}");

    let v = rpc(&w, &owner, "tools/call", json!({"name": "wx__show"})).await;
    let r = &v["result"];
    assert_eq!(r["content"][0]["text"], "sunny", "{v}");
    assert_eq!(r["content"][1]["uri"], NAMESPACED, "{v}");
    assert_eq!(r["content"][2]["resource"]["uri"], NAMESPACED, "{v}");
    assert_eq!(
        r["structuredContent"],
        json!({"temp_c": 21, "sky": "sunny"}),
        "{v}"
    );

    assert!(listed(&w, &owner).await.contains(&NAMESPACED.to_string()));
    let v = rpc(&w, &owner, "resources/read", json!({"uri": NAMESPACED})).await;
    assert_eq!(v["result"]["contents"][0]["uri"], NAMESPACED, "{v}");
    assert_eq!(v["result"]["contents"][0]["text"], page(CARD), "{v}");
    assert_eq!(wx.reads(), [CARD]);
}

/// An app-only tool (`visibility: ["app"]`) is on `/mcp` for a session that
/// declared itself an MCP Apps host, and for no other; and no run offers
/// it to a model: not `/v1/mcp/servers/{label}`, not `/v1/responses`; a
/// label with nothing else says why.
#[tokio::test]
async fn app_only_tools_are_on_mcp_and_never_offered_to_a_model() {
    let w = world(|_| {}).await;
    let _wx = server(&w, "wx", "wx", weather()).await;
    let only = Apps {
        tools: json!([{"name": "refresh", "inputSchema": {"type": "object"},
                       "_meta": {"ui": {"visibility": ["app"]}}}]),
        resources: json!([]),
        templates: json!([]),
        ..Apps::default()
    };
    let _ao = server(&w, "ao", "ao", only).await;
    connect_all(&w).await;
    let owner = w.gw.client();

    let names = |v: Value| -> Vec<String> {
        v["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{v}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let sid = apps_session(&w, &owner).await;
    let host = names(mcp_rpc(&w, &owner, &sid, "tools/list", json!({})).await);
    assert!(
        ["wx__show", "wx__refresh", "ao__refresh"]
            .iter()
            .all(|n| host.iter().any(|h| h == n)),
        "{host:?}"
    );
    let plain = names(rpc(&w, &owner, "tools/list", json!({})).await);
    assert!(plain.iter().any(|n| n == "wx__show"), "{plain:?}");
    assert!(
        !plain.iter().any(|n| n.ends_with("__refresh")),
        "a client that is no apps host was offered an app-only tool: {plain:?}"
    );

    let (s, v) = crate::device_chat::get(&w, &owner, "/v1/mcp/servers/wx").await;
    assert_eq!(s, 200, "{v}");
    let wire: Vec<&str> = v["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(wire, ["show"], "{v}");

    w.chat.push(Turn::text(&["ok"]));
    let body = json!({"model": "chatty", "input": "hi",
                      "tools": [{"type": "mcp", "server_label": "wx"},
                                {"type": "mcp", "server_label": "ao"}]});
    let v: Value = owner
        .post(format!("{}/v1/responses", w.gw))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listings: Vec<&Value> = v["output"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .filter(|i| i["type"] == "mcp_list_tools")
        .collect();
    let of = |label: &str| {
        listings
            .iter()
            .find(|l| l["server_label"] == label)
            .copied()
            .unwrap_or_else(|| panic!("no listing of {label}: {v}"))
    };
    let tools: Vec<&str> = of("wx")["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["wx__show"], "{v}");
    assert!(
        of("ao")["error"].to_string().contains("app views only"),
        "{}",
        of("ao")
    );
    let sent = w.chat.seen.chat(0)["tools"].to_string();
    assert!(
        !sent.contains("refresh"),
        "the model was offered it: {sent}"
    );
}

/// A Chat turn's `tool` result frame carries the label the tool came from,
/// the namespaced UI resource it links to, its structured content and its
/// content blocks; a tool with no UI carries none. Its `ready` frame carries
/// the label, the resource, the call id and whether it waits.
#[tokio::test]
async fn a_chat_tool_frame_carries_what_an_apps_host_needs() {
    let w = world(|_| {}).await;
    let _wx = server(&w, "wx", "wx", weather()).await;
    let plain = Apps {
        tools: json!([{"name": "ping", "inputSchema": {"type": "object"}}]),
        call_result: json!({"content": [{"type": "text", "text": "pong"}]}),
        resources: json!([]),
        templates: json!([]),
    };
    let _p = server(&w, "plain", "pl", plain).await;
    let owner = w.gw.client();
    let tid = w.thread("chatty", json!({})).await;
    w.set(
        tid,
        json!({"mcp_tools": [{"server_label": "wx"}, {"server_label": "pl"}]}),
    )
    .await;
    w.chat.push(calls(
        &[(0, "c1", "wx__show", "{}"), (1, "c2", "pl__ping", "{}")],
        "tool_calls",
    ));
    w.chat.push(Turn::text(&["done"]));
    let (s, frames) = sse(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "weather?"}),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let result = |name: &str| {
        frames
            .iter()
            .find(|(e, d)| e == "tool" && d["event"] == "result" && d["name"] == name)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| panic!("no result frame for {name}: {frames:?}"))
    };
    let show = result("wx__show");
    assert_eq!(show["server_label"], "wx", "{show}");
    assert_eq!(show["ui_resource"], NAMESPACED, "{show}");
    assert_eq!(
        show["structured_content"],
        json!({"temp_c": 21, "sky": "sunny"}),
        "{show}"
    );
    assert_eq!(show["is_error"], false, "{show}");
    let ping = result("pl__ping");
    assert_eq!(ping["server_label"], "pl", "{ping}");
    assert_eq!(ping["ui_resource"], Value::Null, "{ping}");
    assert_eq!(ping["structured_content"], Value::Null, "{ping}");
    assert_eq!(
        ping["content"],
        json!([{"type": "text", "text": "pong"}]),
        "{ping}"
    );

    // The `ready` frame says it before the call runs (§7.5): whether it
    // waits for an approval, its label, its view and its call id.
    let ready = |name: &str| {
        frames
            .iter()
            .find(|(e, d)| e == "tool" && d["event"] == "ready" && d["name"] == name)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| panic!("no ready frame for {name}: {frames:?}"))
    };
    let show = ready("wx__show");
    assert_eq!(show["call_id"], "c1", "{show}");
    assert_eq!(show["server_label"], "wx", "{show}");
    assert_eq!(show["needs_approval"], false, "{show}");
    assert_eq!(show["ui_resource"], NAMESPACED, "{show}");
    assert_eq!(show["arguments"], json!({}), "{show}");
    let ping = ready("pl__ping");
    assert_eq!(ping["ui_resource"], Value::Null, "{ping}");
    assert_eq!(ping["call_id"], "c2", "{ping}");
}
