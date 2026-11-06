//! Self-admin tools over `/mcp/admin` — the read tools: status and upstreams.

use super::*;

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_the_gateway() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let p = payload(&call(&base, &sid, "lmgw__status", json!({})).await);

    assert!(p["version"].is_string());
    assert!(p["uptime_seconds"].is_number());
    assert_eq!(p["self_admin"], "read_only");
    // No models are configured in this test's snapshot, so the per-model
    // runtime list (per-model-containers §3.2/§8, replacing the old fixed
    // three-container list) is empty rather than absent.
    assert_eq!(p["runtime"].as_array().unwrap().len(), 0);
    assert!(p["requests"]["total"].is_number());
    assert!(p["counts"]["upstreams"].is_number());
}

#[tokio::test]
async fn upstreams_never_returns_an_api_key() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({
            "action": "create", "name": "secret-up", "protocol": "openai_compatible",
            "base_url": "https://api.example.com/v1", "api_key": "sk-super-secret",
        }),
    )
    .await;

    let listed = call(&base, &sid, "lmgw__upstreams", json!({})).await;
    let raw = text_of(&listed);
    assert!(
        !raw.contains("sk-super-secret"),
        "the API key leaked into the tool output: {raw}"
    );
    let p: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(p["upstreams"][0]["has_api_key"], true);
    assert!(p["upstreams"][0].get("api_key").is_none());
}
