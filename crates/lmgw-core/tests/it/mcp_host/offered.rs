//! A run calls only the tools it offered the model (review finding 1): a
//! name a model emits that the run never listed — a guess, a prompt
//! injection — reaches no server, a device's least of all, whose calls carry
//! the run's caller as trusted `_meta`. On a Chat turn, an in-process agent
//! run and the executor itself; and an agent's reach into a device label,
//! with and without the label, through a real run and a real `/mcp`.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use super::{host_world, linked, mcp_rpc, mcp_session, mcp_tools, World};
use crate::device_chat::{bearer, chat_thread, op, pair, post, sse};
use crate::support::mcp_stub::{echo_stub, register, McpStub};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_mcp::calls;

/// An HTTP MCP server `name` (prefix the same) offering `echo`.
async fn server(w: &World, name: &str) -> McpStub {
    let stub = echo_stub().await;
    register(&w.state, name, name, &stub.url, true, None).await;
    stub
}

/// `thread`'s settings: these labels.
async fn attach(w: &World, client: &reqwest::Client, tid: i64, labels: &[&str]) {
    let tools: Vec<Value> = labels.iter().map(|l| json!({"server_label": l})).collect();
    let (s, v) = post(
        w,
        client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": tools}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

/// No `tools/call` reached the device by now.
async fn device_saw_no_call(dev: &mut super::FakeDevice) {
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(dev.calls.try_recv().is_err(), "the device was called");
}

/// A Chat turn whose model emits a device's tool and a server's tool, neither
/// on the thread: neither runs. The thread offers `web` only.
#[tokio::test]
async fn a_chat_turn_calls_no_tool_it_did_not_offer() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let web = server(&w, "web").await;
    let gh = server(&w, "gh").await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    attach(&w, &owner, tid, &["web"]).await;

    for name in ["desktop__echo", "gh__echo"] {
        let turn = match name {
            "desktop__echo" => calls(&[(0, "c1", "desktop__echo", "{}")], "tool_calls"),
            _ => calls(&[(0, "c1", "gh__echo", "{}")], "tool_calls"),
        };
        w.chat.push(turn);
        w.chat.push(Turn::text(&["done"]));
        let (s, said) = sse(
            &w,
            &owner,
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": format!("call {name}")}),
        )
        .await;
        assert_eq!(s, 200, "{said:?}");
    }
    device_saw_no_call(&mut dev).await;
    assert!(gh.calls().is_empty(), "gh was called: {:?}", gh.calls());
    assert!(
        web.calls().is_empty(),
        "web was not asked for: {:?}",
        web.calls()
    );
}

/// The executor itself, the backstop under every run: a name it was not
/// offered is refused with why, and reaches neither a device nor a server;
/// an offered one runs where it was listed from.
#[tokio::test]
async fn the_executor_refuses_a_name_it_was_not_offered() {
    use lmgw_core::agent::ToolExecutor;
    use lmgw_core::mcp::exec::McpExecutor;
    use lmgw_core::proxy::RequestCtx;

    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let gh = server(&w, "gh").await;
    w.state.mcp.list_tools(&w.state.snapshot()).await;
    let device_row = super::device_row(&w, d.id).id;

    let exec = McpExecutor::new(w.state.clone(), RequestCtx::default())
        .with_listed(HashMap::from([("desktop__echo".to_string(), device_row)]));
    for name in ["gh__echo", "desktop__other"] {
        let out = exec.call(name, &json!({})).await;
        let (text, _) = lmgw_core::ir::flatten_tool_result(&out.blocks);
        assert!(out.is_error, "{name}: {text}");
        assert!(text.contains("was not offered to this run"), "{text}");
    }
    let bare = McpExecutor::new(w.state.clone(), RequestCtx::default());
    assert!(bare.call("desktop__echo", &json!({})).await.is_error);
    device_saw_no_call(&mut dev).await;
    assert!(gh.calls().is_empty());

    let out = exec.call("desktop__echo", &json!({})).await;
    assert!(!out.is_error, "{:?}", out.blocks);
    assert_eq!(dev.next_call().await["params"]["name"], "echo");
}

/// An agent manifest: `tools` and a list phase whose source is `source`.
fn agent_doc(id: &str, tools: Value, source: &str) -> String {
    json!({
        "schema_version": 1,
        "id": id,
        "name": id,
        "model": {"alias": "chatty"},
        "tools": tools,
        "run": {"kind": "batch",
            "source": {"tool": source, "args": {}},
            "items_path": "/items",
            "item": {"id": "{{item.id}}", "columns": {}}}
    })
    .to_string()
}

async fn install(w: &World, doc: &str) {
    let resp =
        w.gw.client()
            .post(format!("{}/api/agents/import?replace=1", w.gw))
            .header("content-type", "application/json")
            .body(doc.to_string())
            .send()
            .await
            .unwrap();
    let status = resp.status().as_u16();
    assert_eq!(status, 200, "import: {}", resp.text().await.unwrap());
}

/// Run agent `id`'s list phase to its end: the job's status and error.
async fn run_list(w: &World, id: &str) -> (String, Option<String>) {
    let (s, v) = op(w, "agent_run", json!({"id": id, "phase": "list"})).await;
    assert_eq!(s, 200, "{v}");
    let job = v["job_id"].as_i64().unwrap();
    for _ in 0..500 {
        let row = lmgw_core::store::get_job(&w.state.db, job)
            .await
            .unwrap()
            .unwrap();
        if !matches!(row.status.as_str(), "queued" | "running") {
            return (row.status, row.error);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {job} never finished");
}

/// An agent's in-process run reaches a device's tool only through its
/// manifest's label: without it, a step naming the tool fails and the
/// device sees nothing — nor does a server the manifest leaves out; with
/// it, the call reaches the device as the gateway's own run.
#[tokio::test]
async fn an_agent_run_reaches_a_device_tool_only_through_its_label() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let gh = server(&w, "gh").await;
    let without = json!([{"label": "lmgw", "allowed": ["lmgw__models"]}]);
    for (id, source) in [("probe", "desktop__echo"), ("probe2", "gh__echo")] {
        install(&w, &agent_doc(id, without.clone(), source)).await;
        let (status, error) = run_list(&w, id).await;
        assert_eq!(status, "failed", "{id}: {error:?}");
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains(&format!("no tool named '{source}'"))),
            "{id}: {error:?}"
        );
    }
    device_saw_no_call(&mut dev).await;
    assert!(gh.calls().is_empty());

    install(
        &w,
        &agent_doc("reacher", json!([{"label": "desktop"}]), "desktop__echo"),
    )
    .await;
    let _ = run_list(&w, "reacher").await;
    let call = dev.next_call().await;
    assert_eq!(call["params"]["name"], "echo", "{call}");
    assert_eq!(
        call["params"]["_meta"]["lmgw/caller"]["kind"], "gateway",
        "{call}"
    );
}

/// An agent's token on `/mcp`: the device label is listed and callable
/// only when the manifest names it (L16).
#[tokio::test]
async fn an_agent_token_reaches_a_device_label_only_through_its_manifest() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    install(
        &w,
        &agent_doc("reacher", json!([{"label": "desktop"}]), "desktop__echo"),
    )
    .await;
    install(
        &w,
        &agent_doc(
            "probe",
            json!([{"label": "lmgw", "allowed": ["lmgw__models"]}]),
            "lmgw__models",
        ),
    )
    .await;
    let token = |id: &'static str| {
        let w = &w;
        async move {
            let (s, v) = op(w, "agent_token_get", json!({"id": id})).await;
            assert_eq!(s, 200, "{v}");
            bearer(v["token"].as_str().unwrap())
        }
    };
    let reacher = token("reacher").await;
    let probe = token("probe").await;

    assert!(mcp_tools(&w, &reacher)
        .await
        .contains(&"desktop__echo".to_string()));
    let sid = mcp_session(&w, &reacher).await;
    let v = mcp_rpc(
        &w,
        &reacher,
        &sid,
        "tools/call",
        json!({"name": "desktop__echo"}),
    )
    .await;
    assert_eq!(v["result"]["content"][0]["text"], "ran echo", "{v}");
    let call = dev.next_call().await;
    assert_eq!(
        call["params"]["_meta"]["lmgw/caller"],
        json!({"kind": "agent", "name": "reacher"}),
        "{call}"
    );

    assert!(!mcp_tools(&w, &probe)
        .await
        .contains(&"desktop__echo".to_string()));
    let sid = mcp_session(&w, &probe).await;
    let v = mcp_rpc(
        &w,
        &probe,
        &sid,
        "tools/call",
        json!({"name": "desktop__echo"}),
    )
    .await;
    assert!(v["result"]["content"].is_null(), "{v}");
    assert!(
        v.to_string().contains("a paired device's hosted tools"),
        "{v}"
    );
    device_saw_no_call(&mut dev).await;
}

/// A device's Chat turn on its thread holding another device's label it no
/// longer reaches: the label is reported, the turn runs with the rest, and
/// a model naming that device's tool reaches nothing.
#[tokio::test]
async fn a_device_turn_on_a_thread_holding_another_device_s_label() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let _web = server(&w, "web").await;
    let phone = pair(
        &w,
        "phone",
        json!({"tool_scope_mode": "allow", "tool_scope_patterns": "desktop__*\nweb__*"}),
    )
    .await;
    let tid = chat_thread(&w, &phone.client, "chatty").await;
    attach(&w, &phone.client, tid, &["desktop", "web"]).await;
    // The owner narrows the phone: the label stays on its thread.
    let (s, v) = op(
        &w,
        "key_set",
        json!({"id": phone.id, "tool_scope_patterns": "web__*"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    w.chat
        .push(calls(&[(0, "c1", "desktop__echo", "{}")], "tool_calls"));
    w.chat.push(Turn::text(&["done"]));
    let (s, said) = sse(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "look at the desktop"}),
    )
    .await;
    assert_eq!(s, 200);
    let error = said
        .iter()
        .find(|(e, _)| e == "error")
        .unwrap_or_else(|| panic!("the label is reported: {said:?}"));
    assert!(
        error.1["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("MCP server 'desktop'")),
        "{said:?}"
    );
    // The model was offered `web`'s tool and not the desktop's.
    let offered: Vec<String> = w.chat.seen.chat(0)["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(offered, vec!["web__echo".to_string()], "{offered:?}");
    device_saw_no_call(&mut dev).await;
}
