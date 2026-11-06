//! Self-admin tools over `/mcp/admin` — self-admin calls in the request log.

use super::*;

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn self_admin_calls_are_written_to_the_request_log() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "create", "name": "audited", "protocol": "openai_compatible",
                "base_url": "https://api.acme.test/v1" }),
    )
    .await;
    // A refused call must be auditable too — that's the one you most want to see.
    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "delete", "id": 99999 }),
    )
    .await;

    let rows = store::query_logs(
        &inspect.db,
        &store::LogFilter {
            alias: None,
            upstream_name: None,
            errors_only: false,
            limit: 50,
            before_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let calls: Vec<_> = rows
        .iter()
        .filter(|r| r.mcp_tool.as_deref() == Some("lmgw__upstream_set"))
        .collect();
    assert_eq!(calls.len(), 2, "both calls should be logged");
    assert!(calls.iter().all(|r| r.ingress_proto == "mcp"));
    assert!(calls
        .iter()
        .all(|r| r.upstream_name.as_deref() == Some("lmgw (self-admin)")));

    // The failure is recorded as a failure, with its reason.
    let failed = calls
        .iter()
        .find(|r| r.status != 200)
        .expect("a failed row");
    assert_eq!(failed.error_kind.as_deref(), Some("tool_error"));
    assert!(failed.error_msg.as_deref().unwrap().contains("99999"));
}
