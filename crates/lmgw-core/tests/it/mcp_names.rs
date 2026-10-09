//! Two MCP servers that offer a tool of one name (client-apps design §7.5;
//! the owner, 2026-10-09): neither is refused or shadowed, the colliding
//! tools take their server's prefix (`mcp::names`). On `/mcp` and in a Chat
//! turn: each name routes to its own server, which receives its own tool
//! name; the bare name says where the tools went; a server reaped since
//! keeps the other's name, and the lists are stored; the owner's approval
//! rules hold in either spelling, and the frames name the tool as the model
//! called it.

use serde_json::{json, Value};

use crate::device_chat::{bearer, op, sse};
use crate::mcp_resources::rpc;
use crate::realtime_chat_thread::{world, World};
use crate::support::mcp_stub::{answer, answering, register, McpStub};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_mcp::calls;

/// A bare server `name` offering `lookup` (and `own_<name>`), answering
/// with its own name.
async fn bare(w: &World, name: &'static str) -> McpStub {
    let tools = json!([
        {"name": "lookup", "inputSchema": {"type": "object"}},
        {"name": format!("own_{name}"), "inputSchema": {"type": "object"}}
    ]);
    let stub = answering(
        tools,
        false,
        answer(move |tool, _| async move {
            json!({"content": [{"type": "text", "text": format!("{name}:{tool}")}]})
        }),
    )
    .await;
    register(&w.state, name, "", &stub.url, true, None).await;
    stub
}

fn names(v: &Value) -> Vec<String> {
    let mut n: Vec<String> = v["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .filter(|n| !n.starts_with("docs__") && !n.starts_with("kb__"))
        .collect();
    n.sort();
    n
}

/// `/mcp` lists both under their server's prefix and the other tools as
/// they were; each call goes to its own server under the server's own name;
/// the bare name is no tool any more, and says which are.
#[tokio::test]
async fn two_bare_servers_tools_of_one_name_both_take_their_prefix() {
    let w = world(|_| {}).await;
    let (alpha, beta) = (bare(&w, "alpha").await, bare(&w, "beta").await);
    let owner = w.gw.client();

    let v = rpc(&w, &owner, "tools/list", json!({})).await;
    assert_eq!(
        names(&v),
        ["alpha__lookup", "beta__lookup", "own_alpha", "own_beta"]
    );

    let v = rpc(&w, &owner, "tools/call", json!({"name": "beta__lookup"})).await;
    assert_eq!(v["result"]["content"][0]["text"], "beta:lookup", "{v}");
    assert!(alpha.calls().is_empty());
    assert_eq!(beta.calls()[0].0, "lookup", "the server's own name");

    let v = rpc(&w, &owner, "tools/call", json!({"name": "lookup"})).await;
    assert_eq!(v["error"]["code"], -32601, "{v}");
    let m = v["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("alpha__lookup, beta__lookup") && m.contains("list the tools again"),
        "{m}"
    );
    assert_eq!(alpha.calls().len() + beta.calls().len(), 1);
}

/// Every tool of a registered server says whose it is in
/// `_meta["lmgw/server"]` — its label, its name, the tool's own name on the
/// server — so a host routes a view's call of a tool a collision moved;
/// what the server put in `_meta` stays, its own `lmgw/server` does not;
/// lmgw's own toolsets carry none.
#[tokio::test]
async fn a_listed_tool_says_which_server_it_belongs_to() {
    use lmgw_api_types::mcp_apps::ToolServer;
    let w = world(|_| {}).await;
    let (_a, _b) = (bare(&w, "alpha").await, bare(&w, "beta").await);
    let tools = json!([{"name": "card", "inputSchema": {"type": "object"},
        "_meta": {"ui": {"resourceUri": "ui://wx/card"},
                  "lmgw/server": {"label": "alpha", "name": "alpha", "tool": "lookup"}}}]);
    let gamma = answering(
        tools,
        false,
        answer(|_, _| async { json!({"content": []}) }),
    )
    .await;
    register(&w.state, "gamma", "g", &gamma.url, true, None).await;
    let owner = w.gw.client();

    let v = rpc(&w, &owner, "tools/list", json!({})).await;
    let listed = v["result"]["tools"].as_array().unwrap();
    let of = |name: &str| -> &Value {
        listed
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("no {name} in {v}"))
    };
    let server = |label: &str, name: &str, tool: &str| ToolServer {
        label: label.into(),
        name: name.into(),
        tool: tool.into(),
    };
    // Moved by the collision: the server's name says whose.
    assert_eq!(
        ToolServer::of_tool(of("alpha__lookup")),
        Some(server("alpha", "alpha", "lookup"))
    );
    assert_eq!(
        ToolServer::of_tool(of("beta__lookup")),
        Some(server("beta", "beta", "lookup"))
    );
    // Not moved, too: one lookup whatever the name.
    assert_eq!(
        ToolServer::of_tool(of("own_beta")),
        Some(server("beta", "beta", "own_beta"))
    );
    let card = of("g__card");
    assert_eq!(
        ToolServer::of_tool(card),
        Some(server("g", "gamma", "card")),
        "lmgw's stamp, not the server's: {card}"
    );
    assert_eq!(card["_meta"]["ui"]["resourceUri"], "ui://g__wx/card");
    for t in listed {
        let name = t["name"].as_str().unwrap();
        if name.starts_with("docs__") || name.starts_with("kb__") {
            assert!(ToolServer::of_tool(t).is_none(), "{t}");
        }
    }
}

/// A server reaped since claims its tools' names still: the other's tool
/// keeps its prefix. What each listed is stored for the next start.
#[tokio::test]
async fn a_reaped_server_keeps_its_claim_and_the_lists_are_stored() {
    let w = world(|_| {}).await;
    let (_a, _b) = (bare(&w, "alpha").await, bare(&w, "beta").await);
    let snap = w.state.snapshot();
    w.state.mcp.list_tools(&snap).await;
    let alpha = snap
        .mcp_servers
        .values()
        .find(|s| s.name == "alpha")
        .unwrap()
        .id;
    w.state.mcp.stop_server(alpha).await;

    let agg = w.state.mcp.aggregate(&snap).await;
    let mut exposed: Vec<&String> = agg.reverse.keys().collect();
    exposed.sort();
    assert_eq!(exposed, ["beta__lookup", "own_beta"]);
    assert_eq!(agg.qualified["beta__lookup"], "lookup");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let known = lmgw_core::store::known_tools(&w.state.db).await.unwrap();
        if known.get(&alpha) == Some(&vec!["lookup".to_string(), "own_alpha".to_string()])
            && known.len() == 2
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "stored: {known:?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The `tool` frames of a turn whose `event` is `kind`.
fn tool_frames(frames: &[(String, Value)], kind: &str) -> Vec<Value> {
    frames
        .iter()
        .filter(|(e, d)| e == "tool" && d["event"] == kind)
        .map(|(_, d)| d.clone())
        .collect()
}

/// A Chat thread attaches both labels; the owner's rule gates `lookup` of
/// `alpha` in either spelling, the prefixed or the tool's own: the call
/// waits, its frames say so and name the tool's own name, and `beta`'s runs
/// on `beta` alone.
#[tokio::test]
async fn approval_rules_hold_in_either_spelling() {
    for spelling in ["alpha__lookup", "lookup"] {
        let w = world(|_| {}).await;
        let (alpha, beta) = (bare(&w, "alpha").await, bare(&w, "beta").await);
        let owner = w.gw.client();
        let tid = w.thread("chatty", json!({})).await;
        w.set(
            tid,
            json!({"mcp_tools": [
                {"server_label": "alpha",
                 "require_approval": {"always": {"tool_names": [spelling]}}},
                {"server_label": "beta"}
            ]}),
        )
        .await;

        w.chat
            .push(calls(&[(0, "c1", "beta__lookup", "{}")], "tool_calls"));
        w.chat.push(Turn::text(&["beta answered"]));
        let (s, frames) = sse(
            &w,
            &owner,
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "which?"}),
        )
        .await;
        assert_eq!(s, 200, "{frames:?}");
        let ready = &tool_frames(&frames, "ready")[0];
        assert_eq!(ready["needs_approval"], false, "{ready}");
        assert_eq!(ready["server_label"], "beta", "{ready}");
        let result = &tool_frames(&frames, "result")[0];
        assert_eq!(result["output"], "beta:lookup", "{result}");
        assert_eq!(beta.calls()[0].0, "lookup");

        w.chat
            .push(calls(&[(0, "c2", "alpha__lookup", "{}")], "tool_calls"));
        let (s, frames) = sse(
            &w,
            &owner,
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "and alpha?"}),
        )
        .await;
        assert_eq!(s, 200, "{frames:?}");
        let ready = &tool_frames(&frames, "ready")[0];
        assert_eq!(ready["name"], "alpha__lookup", "{ready}");
        assert_eq!(ready["needs_approval"], true, "{spelling}: {ready}");
        assert_eq!(ready["call_id"], "c2", "{ready}");
        let approval = &tool_frames(&frames, "approval")[0];
        assert_eq!(approval["server_label"], "alpha", "{approval}");
        assert_eq!(approval["name"], "lookup", "{approval}");
        assert!(alpha.calls().is_empty(), "{spelling}: a gated call ran");
    }
}

/// The owner switched `lookup` off while `alpha` alone offered it; `beta`
/// comes to offer it too: both prefixed tools stay off, on `/mcp` and on a
/// call, and the inventory says which switch turns them on.
#[tokio::test]
async fn a_switch_set_before_the_collision_holds_after_it() {
    let w = world(|_| {}).await;
    let _alpha = bare(&w, "alpha").await;
    let owner = w.gw.client();
    let v = rpc(&w, &owner, "tools/list", json!({})).await;
    assert_eq!(names(&v), ["lookup", "own_alpha"]);
    lmgw_core::store::disable_tool(&w.state.db, "lookup", "alpha")
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();

    let beta = bare(&w, "beta").await;
    let v = rpc(&w, &owner, "tools/list", json!({})).await;
    assert_eq!(names(&v), ["own_alpha", "own_beta"]);
    let v = rpc(&w, &owner, "tools/call", json!({"name": "beta__lookup"})).await;
    assert_eq!(v["error"]["code"], -32601, "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("disabled"),
        "{v}"
    );
    assert!(beta.calls().is_empty());

    let inv = lmgw_core::mcp::inventory::list(&w.state).await;
    let entry = inv
        .tools
        .iter()
        .find(|t| t.name == "beta__lookup")
        .unwrap_or_else(|| panic!("{:?}", inv.tools));
    assert_eq!(entry.moved_from.as_deref(), Some("lookup"));
    assert!(!entry.enabled && !entry.available);
    assert!(
        entry
            .reason
            .as_deref()
            .unwrap()
            .contains("switch 'lookup' on"),
        "{:?}",
        entry.reason
    );
}

/// The id of the server called `name`.
fn id_of(w: &World, name: &str) -> i64 {
    w.state
        .snapshot()
        .mcp_servers
        .values()
        .find(|s| s.name == name)
        .unwrap()
        .id
}

/// A client key with tool scope `mode`/`patterns`: a client presenting it.
async fn key(w: &World, name: &str, mode: &str, patterns: &str) -> reqwest::Client {
    let (s, v) = op(
        w,
        "key_create",
        json!({"name": name, "tool_scope_mode": mode, "tool_scope_patterns": patterns}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    bearer(v["plaintext"].as_str().unwrap())
}

/// A key that denies `lookup` reaches neither tool a collision moved from
/// it, on the list or on a call; one that allows `lookup` reaches neither
/// either — an allow names the tool it names, and no other (review 1).
#[tokio::test]
async fn a_key_s_scope_holds_for_a_tool_a_collision_moved() {
    let w = world(|_| {}).await;
    let (alpha, beta) = (bare(&w, "alpha").await, bare(&w, "beta").await);
    let deny = key(&w, "k-deny", "deny", "lookup").await;
    let v = rpc(&w, &deny, "tools/list", json!({})).await;
    assert_eq!(names(&v), ["own_alpha", "own_beta"]);
    for name in ["alpha__lookup", "beta__lookup"] {
        let v = rpc(&w, &deny, "tools/call", json!({"name": name})).await;
        assert_eq!(v["error"]["code"], -32601, "{name}: {v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("a deny list"),
            "{v}"
        );
    }
    let allow = key(&w, "k-allow", "allow", "lookup\nown_*").await;
    let v = rpc(&w, &allow, "tools/list", json!({})).await;
    assert_eq!(names(&v), ["own_alpha", "own_beta"]);
    assert!(alpha.calls().is_empty() && beta.calls().is_empty());
}

/// The owner switched `alpha__lookup` off while `beta` offered `lookup`
/// too; `beta` is deleted, the collision ends: alpha's tool, `lookup`
/// again, stays off, the inventory says which switch turns it on, and
/// switching `lookup` on says it is still off (review 2).
#[tokio::test]
async fn a_switch_set_on_the_moved_name_holds_after_the_collision() {
    let w = world(|_| {}).await;
    let (alpha, _beta) = (bare(&w, "alpha").await, bare(&w, "beta").await);
    let owner = w.gw.client();
    let v = rpc(&w, &owner, "tools/list", json!({})).await;
    assert!(names(&v).contains(&"alpha__lookup".to_string()), "{v}");
    let (s, v) = op(
        &w,
        "tool_set",
        json!({"name": "alpha__lookup", "enabled": false}),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    let beta = id_of(&w, "beta");
    let (s, v) = op(
        &w,
        "mcp_server_set",
        json!({"action": "delete", "id": beta}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let v = rpc(&w, &owner, "tools/list", json!({})).await;
    assert_eq!(names(&v), ["own_alpha"]);
    let v = rpc(&w, &owner, "tools/call", json!({"name": "lookup"})).await;
    assert!(
        v["error"]["message"].as_str().unwrap().contains("disabled"),
        "{v}"
    );
    assert!(alpha.calls().is_empty());

    let inv = lmgw_core::mcp::inventory::list(&w.state).await;
    let entry = inv.tools.iter().find(|t| t.name == "lookup").unwrap();
    assert!(!entry.enabled && !entry.available && entry.moved_from.is_none());
    assert!(
        entry
            .reason
            .as_deref()
            .unwrap()
            .contains("switch 'alpha__lookup' on"),
        "{:?}",
        entry.reason
    );
    let (_, v) = op(&w, "tool_set", json!({"name": "lookup", "enabled": true})).await;
    assert!(v["message"].as_str().unwrap().contains("still off"), "{v}");
    let (_, v) = op(
        &w,
        "tool_set",
        json!({"name": "alpha__lookup", "enabled": true}),
    )
    .await;
    assert_eq!(v["ok"], true, "{v}");
    let v = rpc(&w, &owner, "tools/call", json!({"name": "lookup"})).await;
    assert_eq!(v["result"]["content"][0]["text"], "alpha:lookup", "{v}");
}

/// A moved tool's inventory entry names who else claims its name; once
/// that server is not connected, its claim is its last listing.
#[tokio::test]
async fn the_inventory_names_who_claims_a_moved_name() {
    let w = world(|_| {}).await;
    let (_a, _b) = (bare(&w, "alpha").await, bare(&w, "beta").await);
    let snap = w.state.snapshot();
    w.state.mcp.list_tools(&snap).await;
    let inv = lmgw_core::mcp::inventory::list(&w.state).await;
    let live = inv
        .tools
        .iter()
        .find(|t| t.name == "alpha__lookup")
        .and_then(|t| t.moved_reason.clone())
        .unwrap_or_else(|| panic!("{:?}", inv.tools));
    assert!(
        live.contains("server 'beta' offers") && !live.contains("not connected"),
        "{live}"
    );
    let beta = id_of(&w, "beta");
    w.state.mcp.stop_server(beta).await;
    let agg = w.state.mcp.aggregate(&snap).await;
    assert_eq!(
        agg.moved_by["alpha__lookup"],
        [lmgw_core::mcp::Claimant::Server {
            id: beta,
            name: "beta".into(),
            connected: false
        }]
    );
}

/// A rule written in the name a collision gave a tool still gates it once
/// the collision has ended and the tool has its own name again.
#[tokio::test]
async fn a_rule_in_the_moved_spelling_gates_the_tool_after_the_collision() {
    let w = world(|_| {}).await;
    let alpha = bare(&w, "alpha").await;
    let owner = w.gw.client();
    let tid = w.thread("chatty", json!({})).await;
    w.set(
        tid,
        json!({"mcp_tools": [
            {"server_label": "alpha",
             "require_approval": {"always": {"tool_names": ["alpha__lookup"]}}}
        ]}),
    )
    .await;
    w.chat
        .push(calls(&[(0, "c1", "lookup", "{}")], "tool_calls"));
    let (s, frames) = sse(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "look"}),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let ready = &tool_frames(&frames, "ready")[0];
    assert_eq!(ready["name"], "lookup", "{ready}");
    assert_eq!(ready["needs_approval"], true, "{ready}");
    assert!(alpha.calls().is_empty(), "a gated call ran");
}
