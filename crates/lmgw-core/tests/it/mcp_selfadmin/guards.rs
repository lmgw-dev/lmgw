//! Self-admin tools over `/mcp/admin` — the two guards: the gateway's own endpoint and the reserved prefix.

use super::*;

// ---------------------------------------------------------------------------
// The two guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registering_the_gateways_own_mcp_endpoint_is_refused() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let bind = state.snapshot().settings.bind_addr.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let result = call(
        &base,
        &sid,
        "lmgw__mcp_server_set",
        json!({
            "action": "create", "name": "myself", "transport": "http",
            "url": format!("http://{bind}/mcp"),
        }),
    )
    .await;
    assert!(is_error(&result), "self-registration must be refused");
    let msg = text_of(&result);
    assert!(msg.contains("own MCP endpoint"), "{msg}");

    // A different server on the same host is still fine.
    let ok = call(
        &base,
        &sid,
        "lmgw__mcp_server_set",
        json!({
            "action": "create", "name": "elsewhere", "transport": "http",
            "url": "https://mcp.example.com/mcp",
        }),
    )
    .await;
    assert!(!is_error(&ok), "{}", text_of(&ok));
}

#[tokio::test]
async fn a_southbound_server_cannot_claim_the_reserved_prefix() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;

    let result = call(
        &base,
        &sid,
        "lmgw__mcp_server_set",
        json!({
            "action": "create", "name": "impostor", "transport": "http",
            "url": "https://mcp.example.com/mcp", "tool_prefix": "lmgw",
        }),
    )
    .await;
    assert!(is_error(&result));
    assert!(
        text_of(&result).contains("reserved"),
        "{}",
        text_of(&result)
    );
}
