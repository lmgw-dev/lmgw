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

/// Review finding S3 (document-conversion WP6): the page-reading model is
/// the owner's own choice (D5, off until the user picks). A model with
/// self-admin writes that set it to a cloud alias would send every
/// caller's scanned and table pages there; the tool refuses both settings
/// by name, with full self-admin too, and nothing is stored.
#[tokio::test]
async fn the_page_reading_model_is_no_tool_s_to_choose() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;
    for args in [
        json!({ "documents_page_reader": "example-cloud/vision" }),
        json!({ "documents_page_reader_doctags": true }),
        json!({ "retention_days": 3, "documents_page_reader": "example-cloud/vision" }),
    ] {
        let refused = call(&base, &sid, "lmgw__settings_set", args.clone()).await;
        assert!(is_error(&refused), "{args}: {refused}");
        let text = text_of(&refused);
        assert!(
            text.starts_with("documents_page_reader") && text.contains("admin tools"),
            "{text}"
        );
        assert!(text.contains("document pages are sent to"), "{text}");
    }
    let s = inspect.snapshot().settings.clone();
    assert_eq!(s.documents.page_reader, "");
    assert!(!s.documents.page_reader_doctags);
    assert_ne!(s.retention_days, 3, "a refused call changes nothing");
    // The tool says so where a model reads what it may set.
    let tools = list_tools(&base, &sid).await;
    let described = tools
        .iter()
        .find(|t| t["name"] == "lmgw__settings_set")
        .and_then(|t| t["description"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        described.contains("documents_page_reader and documents_page_reader_doctags"),
        "{described}"
    );
}
