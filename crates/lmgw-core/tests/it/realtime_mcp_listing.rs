//! Server-side MCP tools on `/v1/realtime`, the listing (realtime-server-tools
//! design §1.1–§1.3; WP2): the `mcp_list_tools` item and its events in
//! order, a failed label's `error`, the key's tool scope and the owner-only
//! `lmgw` toolset, the targeted connect, the response that waits for a
//! listing in flight, what an update keeps, relists and drops — and the
//! `response.create` rules that look labels up in the session's tool table:
//! unlisted labels, label-only reuse, narrowing, the function/MCP name clash
//! and an `mcp` tool_choice.

use std::time::Duration;

use lmgw_core::config::KeyPolicy;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};

use crate::support::mcp_stub::{echo_stub, held_stub, register};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, open, send, text_session, types, user_text, Turn,
    Ws, KEY,
};

/// A text session on `chatty` presenting `bearer`.
async fn keyed_session(addr: &str, bearer: &str) -> Ws {
    let auth = format!("Bearer {bearer}");
    let mut ws = open(
        addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &auth)],
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    ws
}

/// `session.update` with these `tools`, past its `session.updated`.
async fn set_tools(ws: &mut Ws, tools: Value) {
    send(
        ws,
        json!({"type": "session.update", "session": {"type": "realtime", "tools": tools}}),
    )
    .await;
    let ev = next_event(ws).await;
    assert_eq!(ev["type"], "session.updated", "{ev}");
}

/// One label's listing, from its `conversation.item.added` to its
/// `conversation.item.done` — and the `error` a failed one carries after.
async fn listing(ws: &mut Ws) -> Vec<Value> {
    let mut events = events_until(ws, "conversation.item.done").await;
    if events.iter().any(|e| e["type"] == "mcp_list_tools.failed") {
        events.push(next_event(ws).await);
    }
    events
}

fn tool_names(item: &Value) -> Vec<&str> {
    item["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect()
}

/// The key's tool scope: an allow list.
async fn scope_key(state: &SharedState, patterns: &str) {
    sqlx::query(
        "UPDATE api_keys SET tool_scope_mode = 'allow', tool_scope_patterns = ?1
         WHERE name = 'voice'",
    )
    .bind(patterns)
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// §1.2's order: the item (`tools: []`, retrievable from here on), its
/// `in_progress`, then `completed` and the item again with the tools filled
/// in by their wire names, each with a string description and `null`
/// annotations.
#[tokio::test]
async fn a_label_is_listed_into_an_item_in_order() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "docs"}])).await;
    let events = listing(&mut ws).await;
    assert_eq!(
        types(&events),
        vec![
            "conversation.item.added",
            "mcp_list_tools.in_progress",
            "mcp_list_tools.completed",
            "conversation.item.done",
        ]
    );
    let added = &events[0]["item"];
    assert_eq!(added["type"], "mcp_list_tools");
    assert_eq!(added["server_label"], "docs");
    assert_eq!(added["tools"], json!([]));
    let id = added["id"].as_str().unwrap();
    for e in &events[1..3] {
        assert_eq!(e["item_id"], id, "{e}");
    }
    let done = &events[3]["item"];
    assert_eq!(done["id"], id);
    assert_eq!(tool_names(done), vec!["resolve", "query", "request"]);
    for t in done["tools"].as_array().unwrap() {
        assert!(t["description"].is_string(), "{t}");
        assert_eq!(t.get("annotations"), Some(&Value::Null), "{t}");
        assert_eq!(t["input_schema"]["type"], "object", "{t}");
    }
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": id}),
    )
    .await;
    let got = next_event(&mut ws).await;
    assert_eq!(got["type"], "conversation.item.retrieved");
    assert_eq!(got["item"], *done);
}

/// The SDK retrieves the item on `in_progress`: it is there while the
/// listing still runs, and a response waits for that listing before it
/// renders — the wait timed as `MCP listing` (§1.2).
#[tokio::test]
async fn a_response_waits_for_a_listing_in_flight() {
    let (log, _guard) = crate::common::captured_log::capture_log();
    let fake = chat_fake().await;
    fake.push(Turn::text(&["ok"]));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let slow = held_stub().await;
    register(&state, "slow", "slow", &slow.url, true, None).await;
    let mut ws = text_session(&addr).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "slow"}])).await;
    let added = next_event(&mut ws).await;
    assert_eq!(added["type"], "conversation.item.added", "{added}");
    let id = added["item"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        next_event(&mut ws).await["type"],
        "mcp_list_tools.in_progress"
    );
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": id}),
    )
    .await;
    let got = next_event(&mut ws).await;
    assert_eq!(got["type"], "conversation.item.retrieved", "{got}");
    assert_eq!(got["item"]["tools"], json!([]));

    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(next_event(&mut ws).await["type"], "response.created");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(fake.seen.chat_count(), 0, "it rendered before the listing");

    slow.release();
    let events = events_until(&mut ws, "response.done").await;
    let t = types(&events);
    let completed = t.iter().position(|t| *t == "mcp_list_tools.completed");
    let first_output = t.iter().position(|t| t.starts_with("response.output"));
    assert!(completed.is_some() && completed < first_output, "{t:?}");
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(fake.seen.chat_count(), 1);
    let line = log
        .text()
        .lines()
        .find(|l| l.contains("MCP listing"))
        .map(str::to_string)
        .unwrap_or_else(|| panic!("no timing line with the wait: {}", log.text()));
    let ms: u64 = line
        .split("MCP listing ")
        .nth(1)
        .and_then(|r| r.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{line}"));
    assert!(ms >= 350, "{line}");
}

/// A cancel during the wait ends the response at once; the listing goes on
/// and closes its item, and nothing reaches the model.
#[tokio::test]
async fn a_cancel_during_the_wait_still_works() {
    let fake = chat_fake().await;
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let slow = held_stub().await;
    register(&state, "slow", "slow", &slow.url, true, None).await;
    let mut ws = text_session(&addr).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "slow"}])).await;
    events_until(&mut ws, "mcp_list_tools.in_progress").await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(next_event(&mut ws).await["type"], "response.created");
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "cancelled");

    slow.release();
    let rest = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(
        types(&rest),
        vec!["mcp_list_tools.completed", "conversation.item.done"]
    );
    assert_eq!(tool_names(&rest[1]["item"]), vec!["echo"]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.seen.chat_count(), 0);
}

/// A label that cannot be listed: `.failed`, its item closed with no tools,
/// and an `error` with code `mcp_list_tools_failed` in the resolver's words
/// — then the session answers as before.
#[tokio::test]
async fn a_failed_label_says_why_and_the_session_goes_on() {
    let fake = chat_fake().await;
    fake.push(Turn::text(&["fine"]));
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "nope"}])).await;
    let events = listing(&mut ws).await;
    assert_eq!(
        types(&events),
        vec![
            "conversation.item.added",
            "mcp_list_tools.in_progress",
            "mcp_list_tools.failed",
            "conversation.item.done",
            "error",
        ]
    );
    assert_eq!(events[3]["item"]["tools"], json!([]));
    let e = &events[4]["error"];
    assert_eq!(e["code"], "mcp_list_tools_failed");
    assert_eq!(e["type"], "invalid_request_error");
    let message = e["message"].as_str().unwrap();
    assert!(
        message.contains("'nope'") && message.contains("(available: docs, kb)"),
        "{message}"
    );

    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
}

/// A client key lists what its tool scope reaches: another server is
/// answered like one that does not exist and is never contacted (§1.2,
/// key tool scope design). Only the server a label names connects.
#[tokio::test]
async fn a_client_key_lists_within_its_tool_scope_and_only_the_named_server_connects() {
    let fake = chat_fake().await;
    let (state, addr) = gateway(&fake, true, Some(KeyPolicy::default()), |_| {}).await;
    scope_key(&state, "a__*").await;
    let alpha = echo_stub().await;
    let beta = echo_stub().await;
    register(&state, "alpha", "a", &alpha.url, true, None).await;
    register(&state, "beta", "b", &beta.url, true, None).await;
    let mut ws = keyed_session(&addr, KEY).await;

    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "b"}])).await;
    let events = listing(&mut ws).await;
    assert_eq!(events[2]["type"], "mcp_list_tools.failed", "{events:?}");
    let message = events[4]["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("no MCP server with label 'b'") && message.contains("(available: a)"),
        "{message}"
    );
    // Built-ins connect nothing either.
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "docs"}])).await;
    let events = listing(&mut ws).await;
    assert!(
        events[4]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tool scope"),
        "{events:?}"
    );
    assert_eq!((alpha.hits(), beta.hits()), (0, 0));

    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "a"}])).await;
    let events = listing(&mut ws).await;
    assert_eq!(events[2]["type"], "mcp_list_tools.completed", "{events:?}");
    assert_eq!(tool_names(&events[3]["item"]), vec!["echo"]);
    assert!(alpha.hits() > 0);
    assert_eq!(beta.hits(), 0, "beta was never named");
}

/// The `lmgw` toolset is an owner credential's only (decision 2); for the
/// owner it lists the self-admin tools by their own names.
#[tokio::test]
async fn lmgw_is_refused_to_a_client_key_and_listed_for_the_owner() {
    let fake = chat_fake().await;
    let (state, addr) = gateway(&fake, true, Some(KeyPolicy::default()), |_| {}).await;
    let mut ws = keyed_session(&addr, KEY).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "lmgw"}])).await;
    let events = listing(&mut ws).await;
    assert_eq!(events[2]["type"], "mcp_list_tools.failed", "{events:?}");
    assert!(
        events[4]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("needs an owner credential"),
        "{events:?}"
    );

    let owner = crate::common::dashboard_key(&state);
    let mut ws = keyed_session(&addr, &owner).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "lmgw"}])).await;
    let events = listing(&mut ws).await;
    assert_eq!(events[2]["type"], "mcp_list_tools.completed", "{events:?}");
    let names = tool_names(&events[3]["item"]);
    assert!(names.contains(&"status"), "{names:?}");
    assert!(names.iter().all(|n| !n.starts_with("lmgw__")), "{names:?}");
}

/// An unchanged definition — label-only reuse included — is not listed
/// again; a changed `allowed_tools` is, into a new item; a dropped label
/// loses its tools and keeps its item (§1.2).
#[tokio::test]
async fn an_update_keeps_relists_and_drops_labels() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    let docs = json!({"type": "mcp", "server_label": "docs",
                      "allowed_tools": {"tool_names": ["query", "resolve"]}});
    set_tools(&mut ws, json!([docs])).await;
    let first = listing(&mut ws).await;
    let first_id = first[0]["item"]["id"].as_str().unwrap().to_string();
    assert_eq!(tool_names(&first[3]["item"]), vec!["resolve", "query"]);

    // The same definition, and the label alone: nothing listed. The next
    // event is the answer to the retrieve, not a listing.
    for tools in [
        json!([docs]),
        json!([{"type": "mcp", "server_label": "docs"}, {"type": "function", "name": "f"}]),
    ] {
        set_tools(&mut ws, tools).await;
        send(
            &mut ws,
            json!({"type": "conversation.item.retrieve", "item_id": first_id}),
        )
        .await;
        assert_eq!(
            next_event(&mut ws).await["type"],
            "conversation.item.retrieved"
        );
    }

    set_tools(
        &mut ws,
        json!([{"type": "mcp", "server_label": "docs", "allowed_tools": ["query"]}]),
    )
    .await;
    let second = listing(&mut ws).await;
    assert_ne!(second[0]["item"]["id"], first_id.as_str());
    assert_eq!(tool_names(&second[3]["item"]), vec!["query"]);

    // Dropped: a function may take one of its names now, the choice of it
    // is refused, and both items are still in the conversation.
    set_tools(
        &mut ws,
        json!([{"type": "function", "name": "docs__query"}]),
    )
    .await;
    send(
        &mut ws,
        json!({"type": "response.create", "response": {
            "tool_choice": {"type": "mcp", "server_label": "docs"}}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error", "{e}");
    assert_eq!(e["error"]["param"], "response.tool_choice");
    for id in [first_id.as_str(), second[0]["item"]["id"].as_str().unwrap()] {
        send(
            &mut ws,
            json!({"type": "conversation.item.retrieve", "item_id": id}),
        )
        .await;
        let got = next_event(&mut ws).await;
        assert_eq!(got["item"]["type"], "mcp_list_tools", "{got}");
    }
}

/// `response.create`'s tools may select or narrow a label the session
/// listed — a label alone reuses the session's definition — and name no
/// other (§1.1).
#[tokio::test]
async fn a_response_s_tools_name_only_labels_the_session_listed() {
    let fake = chat_fake().await;
    fake.push(Turn::text(&["one"]));
    fake.push(Turn::text(&["two"]));
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "docs"}])).await;
    listing(&mut ws).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;

    for (tools, param, says) in [
        (
            json!([{"type": "mcp", "server_label": "kb"}]),
            "response.tools[0].server_label",
            "list it in session.update first",
        ),
        (
            json!([{"type": "mcp", "server_label": "docs", "allowed_tools": ["nope"]}]),
            "response.tools[0].allowed_tools",
            "(it listed: resolve, query, request)",
        ),
        (
            json!([{"type": "mcp", "server_label": "docs"},
                   {"type": "function", "name": "docs__query"}]),
            "response.tools[1].name",
            "rename the function",
        ),
    ] {
        send(
            &mut ws,
            json!({"type": "response.create", "event_id": "r1",
                   "response": {"tools": tools}}),
        )
        .await;
        let e = next_event(&mut ws).await;
        assert_eq!(e["type"], "error", "{e}");
        assert_eq!(e["error"]["code"], "invalid_value", "{e}");
        assert_eq!(e["error"]["param"], param, "{e}");
        assert_eq!(e["error"]["event_id"], "r1", "{e}");
        let message = e["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{message}");
    }
    assert_eq!(fake.seen.chat_count(), 0);

    for tools in [
        json!([{"type": "mcp", "server_label": "docs"}]),
        json!([{"type": "mcp", "server_label": "docs", "allowed_tools": ["query"]}]),
    ] {
        send(
            &mut ws,
            json!({"type": "response.create", "response": {"tools": tools}}),
        )
        .await;
        let events = events_until(&mut ws, "response.done").await;
        assert_eq!(
            events.last().unwrap()["response"]["status"],
            "completed",
            "{tools}"
        );
    }
    assert_eq!(fake.seen.chat_count(), 2);
}

/// A function and a server tool of one name could not be told apart
/// (§1.1): an update that adds such a function beside a listed label is
/// refused and changes nothing; one that adds both at once lists the label
/// as failed, saying why.
#[tokio::test]
async fn a_function_and_an_mcp_tool_of_one_name_are_refused() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "docs"}])).await;
    listing(&mut ws).await;

    send(
        &mut ws,
        json!({"type": "session.update", "event_id": "u1", "session": {"type": "realtime",
               "tools": [{"type": "mcp", "server_label": "docs"},
                         {"type": "function", "name": "docs__query"}]}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error", "{e}");
    assert_eq!(e["error"]["param"], "session.tools[1].name");
    assert_eq!(e["error"]["event_id"], "u1");
    assert!(
        e["error"]["message"].as_str().unwrap().contains("'docs'"),
        "{e}"
    );
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime"}}),
    )
    .await;
    let after = next_event(&mut ws).await;
    assert_eq!(
        after["session"]["tools"],
        json!([{"type": "mcp", "server_label": "docs"}])
    );

    set_tools(
        &mut ws,
        json!([{"type": "function", "name": "kb__search"},
               {"type": "mcp", "server_label": "kb"}]),
    )
    .await;
    let events = listing(&mut ws).await;
    assert_eq!(events[2]["type"], "mcp_list_tools.failed", "{events:?}");
    let message = events[4]["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("the session's function 'kb__search'"),
        "{message}"
    );
}

/// `tool_choice: {type: "mcp", server_label, name?}` is looked up in the
/// table: a name the label did not list, or a label the response's tools do
/// not name, is refused like an unknown function; a name it listed — wire
/// or exposed — is taken.
#[tokio::test]
async fn an_mcp_tool_choice_is_looked_up_in_the_table() {
    let fake = chat_fake().await;
    fake.push(Turn::text(&["one"]));
    fake.push(Turn::text(&["two"]));
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "docs"}])).await;
    listing(&mut ws).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;

    for (choice, says) in [
        (
            json!({"type": "mcp", "server_label": "docs", "name": "nope"}),
            "(it has: resolve, query, request)",
        ),
        (
            json!({"type": "mcp", "server_label": "kb"}),
            "not among this response's tools",
        ),
    ] {
        send(
            &mut ws,
            json!({"type": "response.create", "response": {"tool_choice": choice}}),
        )
        .await;
        let e = next_event(&mut ws).await;
        assert_eq!(e["type"], "error", "{e}");
        assert_eq!(e["error"]["param"], "response.tool_choice", "{e}");
        let message = e["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{message}");
    }
    for choice in [
        json!({"type": "mcp", "server_label": "docs", "name": "query"}),
        json!({"type": "mcp", "server_label": "docs", "name": "docs__resolve"}),
    ] {
        send(
            &mut ws,
            json!({"type": "response.create", "response": {"tool_choice": choice}}),
        )
        .await;
        let events = events_until(&mut ws, "response.done").await;
        assert_eq!(
            events.last().unwrap()["response"]["status"],
            "completed",
            "{choice}"
        );
    }
}

/// A label whose listing failed is listed again by an update that names it,
/// unchanged as it is (§1.2): a long-lived client may keep one socket open
/// for hours, and a server down at the first listing may be up now. An update
/// without `tools` leaves it alone, and a listed label is not listed again.
#[tokio::test]
async fn an_update_naming_a_failed_label_lists_it_again() {
    let fake = chat_fake().await;
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let alpha = echo_stub().await;
    register(&state, "alpha", "a", &alpha.url, false, None).await;
    let mut ws = text_session(&addr).await;
    let tools = json!([{"type": "mcp", "server_label": "a"}]);
    set_tools(&mut ws, tools.clone()).await;
    let events = listing(&mut ws).await;
    assert_eq!(events[2]["type"], "mcp_list_tools.failed", "{events:?}");
    let failed_id = events[0]["item"]["id"].as_str().unwrap().to_string();

    sqlx::query("UPDATE mcp_servers SET enabled = 1 WHERE name = 'alpha'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    state.mcp.reconcile(&state.snapshot()).await;

    // No `tools` in the update: nothing listed — the next event answers the
    // retrieve.
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "instructions": "be brief"}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": failed_id}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "conversation.item.retrieved"
    );

    set_tools(&mut ws, tools.clone()).await;
    let events = listing(&mut ws).await;
    assert_eq!(events[2]["type"], "mcp_list_tools.completed", "{events:?}");
    assert_ne!(events[0]["item"]["id"], failed_id.as_str(), "a new item");
    assert_eq!(tool_names(&events[3]["item"]), vec!["echo"]);

    // Listed now: the same definition is kept as it is.
    set_tools(&mut ws, tools).await;
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": failed_id}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "conversation.item.retrieved"
    );
}

/// A listing a later update supersedes closes at once, before the new
/// listing's item is added: `.failed`, no tools, no `error` — and its late
/// result is ignored, since `@openai/agents` replaces a label's tools on
/// every listing it sees done (final review #4). A label dropped while it is
/// listed closes the same way.
#[tokio::test]
async fn a_superseded_listing_closes_at_once_and_its_late_result_is_ignored() {
    let fake = chat_fake().await;
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let slow = held_stub().await;
    register(&state, "slow", "slow", &slow.url, true, None).await;
    let mut ws = text_session(&addr).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "slow"}])).await;
    let first = events_until(&mut ws, "mcp_list_tools.in_progress").await;
    let first_id = first[0]["item"]["id"].as_str().unwrap().to_string();

    set_tools(
        &mut ws,
        json!([{"type": "mcp", "server_label": "slow", "allowed_tools": ["echo"]}]),
    )
    .await;
    let events = events_until(&mut ws, "mcp_list_tools.in_progress").await;
    assert_eq!(
        types(&events),
        vec![
            "mcp_list_tools.failed",
            "conversation.item.done",
            "conversation.item.added",
            "mcp_list_tools.in_progress",
        ]
    );
    assert_eq!(events[0]["item_id"], first_id.as_str());
    assert_eq!(events[1]["item"]["id"], first_id.as_str());
    assert_eq!(events[1]["item"]["tools"], json!([]));
    let second_id = events[2]["item"]["id"].as_str().unwrap().to_string();
    assert_ne!(second_id, first_id);

    slow.release();
    let rest = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(
        types(&rest),
        vec!["mcp_list_tools.completed", "conversation.item.done"]
    );
    assert_eq!(rest[1]["item"]["id"], second_id.as_str());
    assert_eq!(tool_names(&rest[1]["item"]), vec!["echo"]);
    // The first listing's result says nothing: the next event answers the
    // retrieve, and its item is as it was closed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": first_id}),
    )
    .await;
    let got = next_event(&mut ws).await;
    assert_eq!(got["type"], "conversation.item.retrieved", "{got}");
    assert_eq!(got["item"]["tools"], json!([]));

    // Dropped while it is listed.
    let late = held_stub().await;
    register(&state, "late", "late", &late.url, true, None).await;
    set_tools(&mut ws, json!([{"type": "mcp", "server_label": "late"}])).await;
    let added = events_until(&mut ws, "mcp_list_tools.in_progress").await;
    let late_id = added[0]["item"]["id"].as_str().unwrap().to_string();
    set_tools(&mut ws, json!([])).await;
    let closed = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(
        types(&closed),
        vec!["mcp_list_tools.failed", "conversation.item.done"]
    );
    assert_eq!(closed[1]["item"]["id"], late_id.as_str());
    late.release();
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": late_id}),
    )
    .await;
    let got = next_event(&mut ws).await;
    assert_eq!(got["type"], "conversation.item.retrieved", "{got}");
    assert_eq!(got["item"]["tools"], json!([]));
}
