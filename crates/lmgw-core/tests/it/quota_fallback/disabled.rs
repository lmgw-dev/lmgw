//! `disabled` (openai egress design §14.3, decision 28): decided at
//! resolve, for a name that would otherwise not resolve, or an alias whose
//! upstream is disabled.

use super::*;

/// A disabled plain alias: its fallback answers, reason `disabled`; with
/// none, the unknown model it was before.
#[tokio::test]
async fn a_disabled_alias_is_answered_by_its_fallback() {
    let (g, gw) = world().await;
    g.cloud("off", None).await;
    g.cloud("on", None).await;
    exec(&g, "UPDATE models SET enabled = 0 WHERE alias = 'off'").await;

    let resp = ask(&gw, "off").await;
    assert_eq!(resp.status(), 404, "today's answer without a fallback");
    assert!(h(&resp, "x-lmgw-fallback").is_none());

    set_fallback(&g.state, "models", "off", Some("on")).await;
    answered_along(
        ask(&gw, "off").await,
        "off",
        r#""on";reason=disabled"#,
        "from the cloud",
    )
    .await;
}

/// An enabled alias whose upstream row is disabled: the same.
#[tokio::test]
async fn an_alias_on_a_disabled_upstream_is_answered_by_its_fallback() {
    let (g, gw) = world().await;
    g.cloud("off", None).await;
    g.cloud("on", None).await;
    exec(
        &g,
        "UPDATE upstreams SET enabled = 0 WHERE name = 'cloud-off'",
    )
    .await;

    let resp = ask(&gw, "off").await;
    assert!(resp.status().is_server_error(), "{}", resp.status());
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing/disabled upstream"),
        "today's answer: {body}"
    );

    set_fallback(&g.state, "models", "off", Some("on")).await;
    answered_along(
        ask(&gw, "off").await,
        "off",
        r#""on";reason=disabled"#,
        "from the cloud",
    )
    .await;
}

/// A disabled local chat row (public): its fallback answers, and nothing
/// starts.
#[tokio::test]
async fn a_disabled_local_row_is_answered_by_its_fallback() {
    let (g, gw) = world().await;
    g.model("m", 4 * GIB).await;
    g.cloud("on", None).await;
    set_fallback(&g.state, "local_models", "m", Some("on")).await;
    exec(
        &g,
        "UPDATE local_models SET enabled = 0 WHERE model_id = 'm'",
    )
    .await;

    answered_along(
        ask(&gw, "m").await,
        "m",
        r#""on";reason=disabled"#,
        "from the cloud",
    )
    .await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

/// A fallback that is disabled hands the request on to its own fallback,
/// and is named in the chain; one with none ends the walk, saying so.
#[tokio::test]
async fn a_disabled_hop_along_a_chain_goes_on_to_its_own_fallback() {
    let (g, gw) = world().await;
    g.model("m", 4 * GIB).await;
    g.cloud("off", None).await;
    g.cloud("on", None).await;
    set_fallback(&g.state, "local_models", "m", Some("off")).await;
    exec(&g, "UPDATE models SET enabled = 0 WHERE alias = 'off'").await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();

    let resp = ask(&gw, "m").await;
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("(fallback 'off' is disabled)"),
        "{body}"
    );

    set_fallback(&g.state, "models", "off", Some("on")).await;
    answered_along(
        ask(&gw, "m").await,
        "m",
        r#""off";reason=hold, "on";reason=disabled"#,
        "from the cloud",
    )
    .await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

/// A name that resolves keeps resolving as it does: a disabled alias that
/// shares a local model's name falls through to the local model, as before,
/// and its fallback plays no part.
#[tokio::test]
async fn a_disabled_alias_that_shadows_nothing_keeps_its_old_resolution() {
    let (g, gw) = world().await;
    g.model("m", 4 * GIB).await;
    g.cloud("on", None).await;
    let up: i64 = sqlx::query_scalar("SELECT upstream_id FROM models WHERE alias = 'on'")
        .fetch_one(&g.state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id, enabled, fallback) \
         VALUES ('m', ?1, 'gpt', 0, 'on')",
    )
    .bind(up)
    .execute(&g.state.db)
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();

    let resp = ask(&gw, "m").await;
    assert_eq!(resp.status(), 200);
    assert!(h(&resp, "x-lmgw-fallback").is_none());
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "ok from m");
}
