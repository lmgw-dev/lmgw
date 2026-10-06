//! Where a session's server-side call runs (realtime-server-tools design
//! §1.2, §2.4; final review #2): on the server its label was listed from,
//! or nowhere. Two servers without a tool prefix that offer a tool of one
//! name share its exposed name, and the gateway routes it to the first by
//! server name of those connected — so a name listed from `zeta` while
//! `alpha` was not connected is `alpha`'s once `alpha` connects. The call is
//! refused then, visibly, rather than run where the client was never shown
//! it. A listed server that was let go since (idle-reaped) is connected
//! again for the call.

use lmgw_core::state::SharedState;
use serde_json::{json, Value};

use crate::support::mcp_stub::{echo_stub, register};
use crate::support::realtime_fakes::{chat_fake, events_until, gateway, send, user_text, Ws};
use crate::support::realtime_mcp::{calls, tools_session};

/// The id of the registered server `name`.
fn server_id(state: &SharedState, name: &str) -> i64 {
    state
        .snapshot()
        .mcp_servers
        .values()
        .find(|s| s.name == name)
        .map(|s| s.id)
        .unwrap()
}

/// A user turn, `response.create`, and the `mcp_call` its `response.done`
/// carries.
async fn ask_for_call(ws: &mut Ws, text: &str) -> Value {
    send(ws, user_text(text)).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, json!({"type": "response.create"})).await;
    let events = events_until(ws, "response.done").await;
    let done = &events.last().unwrap()["response"];
    done["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_call")
        .cloned()
        .unwrap_or_else(|| panic!("no mcp_call in {done}"))
}

/// `echo` listed from `zeta`, then `alpha` connects and owns the name: the
/// model's call of it is refused with a `tool_execution_error` that says
/// so, and neither server sees a call.
#[tokio::test]
async fn a_name_another_bare_server_took_since_the_listing_is_refused_not_rerouted() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[(0, "call_z1", "echo", r#"{"text":"which"}"#)],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let (alpha, zeta) = (echo_stub().await, echo_stub().await);
    register(&state, "alpha", "", &alpha.url, true, None).await;
    register(&state, "zeta", "", &zeta.url, true, None).await;
    // Only `zeta` is connected for the listing: `echo` is its.
    let tools = json!([{"type": "mcp", "server_label": "zeta"}]);
    let mut ws = tools_session(&addr, None, tools, 1).await;
    assert_eq!(alpha.hits(), 0);

    // `alpha` connects (another caller lists it): first by server name, it
    // owns `echo` now.
    let (status, body) = get(&addr, "/v1/mcp/servers/alpha").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tools"][0]["name"], "echo", "{body}");

    let call = ask_for_call(&mut ws, "which one?").await;
    assert_eq!(call["server_label"], "zeta", "{call}");
    assert_eq!(call["output"], Value::Null, "{call}");
    assert_eq!(call["error"]["type"], "tool_execution_error", "{call}");
    let message = call["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("was listed from server 'zeta'")
            && message.contains("List the label again"),
        "{message}"
    );
    assert!(
        alpha.calls().is_empty(),
        "ran on a server it was not listed from"
    );
    assert!(zeta.calls().is_empty());
}

/// The listed server's connection went (as the idle reaper ends it), and
/// its tools left the aggregate with it: the call connects it again and
/// runs there.
#[tokio::test]
async fn a_listed_server_let_go_since_is_connected_again_for_the_call() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[(0, "call_r1", "a__echo", r#"{"text":"back"}"#)],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let stub = echo_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    let tools = json!([{"type": "mcp", "server_label": "a"}]);
    let mut ws = tools_session(&addr, None, tools, 1).await;
    // As the idle reaper does: the connection goes, and its tools with it.
    state.mcp.stop_server(server_id(&state, "alpha")).await;

    let call = ask_for_call(&mut ws, "still there?").await;
    assert_eq!(call["output"], "echo: back", "{call}");
    assert_eq!(call["error"], Value::Null, "{call}");
    assert_eq!(stub.calls().len(), 1);
}

/// `GET path`, with no credential (auth is off).
async fn get(addr: &str, path: &str) -> (u16, Value) {
    let resp = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}
