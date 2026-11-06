//! The Chat's tool picker and a service agent (chat-plane design D16), on
//! this suite's fake container: reading the agent's label starts nothing.

use lmgw_core::agents::service;
use serde_json::Value;

use super::{doc, get, install, run_count, with_fake, PROVIDES};

/// The Chat's tool picker reads an agent's label without starting it
/// (chat-plane design D16): opening a thread's tool settings is not the ask
/// that runs its container, a turn is. Asleep, the list shows it stopped
/// and the detail says it is not running; once a turn started it, the
/// detail lists its tools, and still starts nothing.
#[tokio::test]
async fn the_chat_s_tool_picker_starts_no_service_agent() {
    let (state, base, calls) = with_fake().await;
    install(&base, &doc("localhost/board:1", 0, 10, PROVIDES)).await;
    let json = |(status, body): (u16, String)| -> (u16, Value) {
        let v = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
        (status, v)
    };

    let (status, list) = json(get(&base, "/chat/api/tools").await);
    assert_eq!(status, 200, "{list}");
    let board = list["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["server_label"] == "board")
        .unwrap_or_else(|| panic!("board is not listed: {list}"))
        .clone();
    assert_eq!(
        (board["kind"].as_str(), board["status"].as_str()),
        (Some("agent"), Some("stopped")),
        "{board}"
    );
    let (status, detail) = json(get(&base, "/chat/api/tools/board").await);
    assert_eq!(
        (status, detail["code"].as_str()),
        (502, Some("mcp_list_tools_failed")),
        "{detail}"
    );
    assert!(
        detail["message"]
            .as_str()
            .is_some_and(|m| m.contains("'board' is not running")),
        "{detail}"
    );
    assert_eq!(run_count(&calls), 0, "the picker started the container");

    // A turn's resolution is the ask that starts it.
    let specs = vec![lmgw_core::ingress::responses::McpToolSpec {
        server_label: "board".to_string(),
        allowed_tools: None,
        require_approval: Default::default(),
        server_url: None,
        connector_id: None,
    }];
    let resolved =
        lmgw_core::mcp::exec::resolve(&state, &specs, &lmgw_core::mcp::scope::ToolScope::gateway())
            .await;
    assert!(resolved.failed.is_empty(), "{:?}", resolved.failed);
    assert_eq!(run_count(&calls), 1);
    let (status, detail) = json(get(&base, "/chat/api/tools/board").await);
    assert_eq!(status, 200, "{detail}");
    assert!(
        detail["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "board__pin"),
        "{detail}"
    );
    assert_eq!(
        run_count(&calls),
        1,
        "a running agent is listed, not started"
    );

    service::stop(&state, "board", "test over").await;
}
