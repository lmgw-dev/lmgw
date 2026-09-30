//! `key_set` — the owner-set half of a key's policy (usage-analytics §4.1) —
//! and the four usage-plane writes the dashboard posts.
//!
//! What these pin down is a wiring bug, not an algorithm. The Keys dialog
//! posted `/api/op/key_set` and the dispatcher had no arm for it, so every
//! Save came back `unknown op 'key_set'` and nothing was written — ever.
//! `price_set`, `price_delete` and `prices_sync` were the same story with
//! complete implementations already behind them, reachable only from the MCP
//! tool plane. So the HTTP hop is tested here, not only the function: a test
//! that called `ops::key_set` directly would have passed throughout the whole
//! period the feature was dead.

use lmgw_core::config::{hash_api_key, ApiKey, ApiKeyKind, ScopeMode};
use lmgw_core::ops::{self, KeyPatch};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common;
use common::{serve, Gw};

async fn gateway() -> SharedState {
    AppState::init_for_tests().await.unwrap()
}

async fn key_named(st: &SharedState, name: &str) -> ApiKey {
    st.snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no key named '{name}'"))
}

async fn plain_key(st: &SharedState, name: &str) -> i64 {
    let id = store::insert_api_key(&st.db, name, &hash_api_key("lmgw-test"))
        .await
        .unwrap();
    st.reload_snapshot().await.unwrap();
    id
}

async fn op(base: &Gw, name: &str, body: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_gateway_key_takes_every_field_the_dialog_offers() {
    let st = gateway().await;
    let id = plain_key(&st, "kilo").await;

    ops::key_set(
        &st,
        KeyPatch {
            id,
            budget_micro: Some(10_000_000),
            budget_period: Some("month".into()),
            scope_mode: Some("allow".into()),
            // Ragged on purpose: a textarea hands back blank lines and stray
            // indentation, and "the same list, retyped" must not read as a
            // change on the next save.
            scope_patterns: Some("kilo/*\n\n   claude-*  \n".into()),
            tool_scope_mode: Some("allow".into()),
            tool_scope_patterns: Some("  docs__*\n\ngithub__search\n".into()),
            rpm_limit: Some(60),
            tpm_limit: Some(120_000),
            concurrency_limit: Some(4),
            expires_at: Some("2026-12-31".into()),
            note: Some("laptop".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let k = key_named(&st, "kilo").await;
    assert_eq!(k.policy.budget_micro, 10_000_000);
    assert_eq!(k.policy.scope_mode, ScopeMode::Allow);
    assert_eq!(k.policy.scope_patterns, "kilo/*\nclaude-*");
    assert_eq!(k.policy.tool_scope_mode, ScopeMode::Allow);
    assert_eq!(k.policy.tool_scope_patterns, "docs__*\ngithub__search");
    assert_eq!(k.policy.rpm_limit, 60);
    assert_eq!(k.policy.tpm_limit, 120_000);
    assert_eq!(k.policy.concurrency_limit, 4);
    assert_eq!(k.policy.expires_at.as_deref(), Some("2026-12-31"));
    assert_eq!(k.note, "laptop");

    // The point of writing a scope is that the gate then reads it.
    assert!(k.policy.admits("kilo/google/gemma-4-31b-it"));
    assert!(!k.policy.admits("gpt-5"));
    assert!(k.policy.admits_tool("docs__query"));
    assert!(!k.policy.admits_tool("github__delete_repo"));

    // Saving the same form again is a no-op that says so, not an error.
    let v = ops::key_set(
        &st,
        KeyPatch {
            id,
            scope_patterns: Some("kilo/*\nclaude-*\n".into()),
            budget_micro: Some(10_000_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(v["changed"].as_array().map(Vec::len), Some(0), "{v}");
}

#[tokio::test]
async fn an_agent_token_takes_a_budget_and_refuses_its_derived_half() {
    let st = gateway().await;
    let id = store::upsert_agent_key(
        &st.db,
        "labeler",
        "agent:labeler",
        &hash_api_key("lmgw-agent-x"),
        "lmgw-agent-x",
        true,
    )
    .await
    .unwrap();
    // The scope `token::write_scope` derives from the manifest.
    store::set_agent_key_scope(&st.db, "labeler", "allow", "qwen3.8")
        .await
        .unwrap();
    st.reload_snapshot().await.unwrap();
    assert_eq!(
        key_named(&st, "agent:labeler").await.kind,
        ApiKeyKind::Agent
    );

    // The money and throughput half is the owner's, and it is enforced.
    ops::key_set(
        &st,
        KeyPatch {
            id,
            budget_micro: Some(5_000_000),
            rpm_limit: Some(30),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        key_named(&st, "agent:labeler").await.policy.budget_micro,
        5_000_000
    );

    // Restating the derived half is not an attempt to change it — the dialog
    // posts every field on every Save, so a refusal here would make the whole
    // row uneditable.
    ops::key_set(
        &st,
        KeyPatch {
            id,
            scope_mode: Some("allow".into()),
            scope_patterns: Some("qwen3.8".into()),
            enabled: Some(true),
            budget_micro: Some(6_000_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        key_named(&st, "agent:labeler").await.policy.budget_micro,
        6_000_000
    );

    // Changing it is refused, and each refusal names where the value lives.
    let e = ops::key_set(
        &st,
        KeyPatch {
            id,
            scope_patterns: Some("gpt-5".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(e.contains("derived"), "{e}");
    assert!(e.contains("agent's page"), "{e}");

    let e = ops::key_set(
        &st,
        KeyPatch {
            id,
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(e.contains("kill switch"), "{e}");

    // Its tools are its manifest's, which is where the refusal points.
    let e = ops::key_set(
        &st,
        KeyPatch {
            id,
            tool_scope_mode: Some("allow".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(e.contains("manifest's tools[]"), "{e}");

    // And a refused save writes nothing at all, including the fields it would
    // have been allowed to write.
    let k = key_named(&st, "agent:labeler").await;
    assert_eq!(k.policy.scope_patterns, "qwen3.8");
    assert!(k.enabled);
}

#[tokio::test]
async fn an_internal_identity_takes_scope_and_budget_and_refuses_what_nothing_reads() {
    let st = gateway().await;
    let id = key_named(&st, "internal:quickdoc-ingest").await.id;

    // The two levers that are really enforced for internal work.
    ops::key_set(
        &st,
        KeyPatch {
            id,
            budget_micro: Some(2_000_000),
            scope_mode: Some("deny".into()),
            scope_patterns: Some("kilo/*".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let k = key_named(&st, "internal:quickdoc-ingest").await;
    assert_eq!(k.policy.budget_micro, 2_000_000);
    assert_eq!(k.policy.scope_mode, ScopeMode::Deny);

    // The four that are checked in the auth middleware an internal identity
    // never reaches. Storing them would be storing a number nothing reads.
    for patch in [
        KeyPatch {
            id,
            rpm_limit: Some(10),
            ..Default::default()
        },
        KeyPatch {
            id,
            concurrency_limit: Some(2),
            ..Default::default()
        },
        KeyPatch {
            id,
            expires_at: Some("2027-01-01".into()),
            ..Default::default()
        },
        KeyPatch {
            id,
            enabled: Some(false),
            ..Default::default()
        },
        // Its tools are attached in process; it never presents a credential
        // for a tool scope to bind.
        KeyPatch {
            id,
            tool_scope_mode: Some("deny".into()),
            ..Default::default()
        },
        KeyPatch {
            id,
            tool_scope_patterns: Some("github__*".into()),
            ..Default::default()
        },
    ] {
        let e = ops::key_set(&st, patch).await.unwrap_err();
        assert!(e.contains("internal identity"), "{e}");
    }
    assert!(key_named(&st, "internal:quickdoc-ingest").await.enabled);
}

#[tokio::test]
async fn a_value_lmgw_cannot_read_later_is_refused_now() {
    let st = gateway().await;
    let id = plain_key(&st, "k").await;

    // `policy::expired` deliberately ignores an unparseable date rather than
    // locking an owner out of their own gateway over a typo — which means a
    // typo accepted here would read as "expiry set" on the page and expire
    // nothing, forever. So it is refused where it is typed.
    let e = ops::key_set(
        &st,
        KeyPatch {
            id,
            expires_at: Some("31.12.2026".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(e.contains("YYYY-MM-DD"), "{e}");

    let e = ops::key_set(
        &st,
        KeyPatch {
            id,
            rpm_limit: Some(-1),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(e.contains("negative"), "{e}");

    let e = ops::key_set(
        &st,
        KeyPatch {
            id,
            budget_period: Some("fortnight".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(e.contains("day|month|total"), "{e}");

    let e = ops::key_set(
        &st,
        KeyPatch {
            id: 9_999,
            budget_micro: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(e.contains("no key with id 9999"), "{e}");
}

#[tokio::test]
async fn the_dashboard_can_reach_all_four_usage_writes() {
    let st = gateway().await;
    let id = plain_key(&st, "kilo").await;
    let base = serve(st.clone()).await;

    let (status, v) = op(
        &base,
        "key_set",
        json!({ "id": id, "budget_micro": 1_000_000 }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(key_named(&st, "kilo").await.policy.budget_micro, 1_000_000);

    let (status, v) = op(
        &base,
        "price_set",
        json!({
            "scope_kind": "alias",
            "scope_key": "kilo/google/gemma-4-31b-it",
            "price_in": 0.09,
            "price_out": 0.34,
        }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let rows = store::list_prices(&st.db).await.unwrap();
    assert_eq!(rows.len(), 1, "the manual row landed");

    let (status, v) = op(&base, "price_delete", json!({ "id": rows[0].id })).await;
    assert_eq!(status, 200, "{v}");
    assert!(store::list_prices(&st.db).await.unwrap().is_empty());

    // A test gateway has no cloud upstream to ask, so the sync asks nobody —
    // and reports that rather than failing.
    let (status, v) = op(&base, "prices_sync", json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["rows_written"], 0, "{v}");
}

#[tokio::test]
async fn the_unpriced_worklist_sees_the_passthrough_models_usage_shows() {
    let st = gateway().await;
    sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url, extra_headers, timeout_ms,
             enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses)
         VALUES ('kilo-gw', 'openai', 'generic', 'http://127.0.0.1:1', '[]', 20000, 1, 1,
                 'kilo', datetime('now'), datetime('now'), 0)",
    )
    .execute(&st.db)
    .await
    .unwrap();
    // Three rollup rows: a passthrough model, an MCP tool call (a name that
    // routes nowhere), and a second passthrough model with fewer requests.
    for (alias, requests) in [
        ("kilo/google/gemma-4-31b-it", 206),
        ("lmgw__status", 17),
        ("kilo/deepseek/deepseek-v4-flash-0731", 14),
    ] {
        sqlx::query(
            "INSERT INTO usage_hourly (bucket_utc, alias, requests) VALUES ('2026-09-19T12', ?1, ?2)",
        )
        .bind(alias)
        .bind(requests)
        .execute(&st.db)
        .await
        .unwrap();
    }
    st.reload_snapshot().await.unwrap();

    let unpriced = ops::unpriced_models(&st).await.unwrap();
    // Busiest first, with what each has already spent that nobody can name.
    assert_eq!(
        unpriced,
        vec![
            ("kilo/google/gemma-4-31b-it".to_string(), 206),
            ("kilo/deepseek/deepseek-v4-flash-0731".to_string(), 14),
        ],
        "a tool call is not a model anyone can price, and never was one"
    );

    // Pricing one takes it off the list — through the same resolution order a
    // real request uses.
    let up_id = st
        .snapshot()
        .upstreams
        .values()
        .find(|u| u.name == "kilo-gw")
        .unwrap()
        .id;
    let (status, v) = {
        let base = serve(st.clone()).await;
        op(
            &base,
            "price_set",
            json!({
                "scope_kind": "upstream_model",
                "scope_key": format!("{up_id}:google/gemma-4-31b-it"),
                "price_in": 0.09,
                "price_out": 0.34,
            }),
        )
        .await
    };
    assert_eq!(status, 200, "{v}");
    let unpriced = ops::unpriced_models(&st).await.unwrap();
    assert_eq!(
        unpriced,
        vec![("kilo/deepseek/deepseek-v4-flash-0731".to_string(), 14)]
    );
}

/// `key_create` takes the four scope fields `key_set` takes, normalises them
/// the same way, and refuses a bad mode *before* a row exists.
#[tokio::test]
async fn key_create_takes_scope_fields_and_normalises_them() {
    let st = gateway().await;
    let gw = serve(st.clone()).await;

    let (status, v) = op(
        &gw,
        "key_create",
        json!({
            "name": "scoped",
            "scope_mode": "allow",
            "scope_patterns": "  claude-*\n\nkilo/*\n",
            "tool_scope_mode": "deny",
            "tool_scope_patterns": " github__*\n",
        }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert!(v["plaintext"].as_str().unwrap().starts_with("lmgw-"), "{v}");
    let k = key_named(&st, "scoped").await;
    assert_eq!(k.policy.scope_mode, ScopeMode::Allow);
    assert_eq!(k.policy.scope_patterns, "claude-*\nkilo/*");
    assert_eq!(k.policy.tool_scope_mode, ScopeMode::Deny);
    assert_eq!(k.policy.tool_scope_patterns, "github__*");

    // A bad mode refuses the create and leaves no key behind.
    let (status, v) = op(
        &gw,
        "key_create",
        json!({ "name": "broken", "tool_scope_mode": "sometimes" }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("tool_scope_mode"),
        "{v}"
    );
    assert!(!st.snapshot().api_keys.iter().any(|k| k.name == "broken"));

    // No scope fields: the row is the default, as before.
    let (status, _) = op(&gw, "key_create", json!({ "name": "plain" })).await;
    assert_eq!(status, 200);
    let k = key_named(&st, "plain").await;
    assert_eq!(k.policy.scope_mode, ScopeMode::All);
    assert_eq!(k.policy.tool_scope_mode, ScopeMode::All);
}

/// An owner key is not scoped: a create that narrows anything is refused with
/// the owner code, one that only restates the default is not.
#[tokio::test]
async fn key_create_refuses_a_scope_on_an_owner_key() {
    let st = gateway().await;
    let gw = serve(st.clone()).await;

    let (status, v) = op(
        &gw,
        "key_create",
        json!({ "name": "boss", "kind": "owner", "tool_scope_mode": "allow",
                "tool_scope_patterns": "docs__*" }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], json!("refuse_owner"), "{v}");
    assert!(!st
        .snapshot()
        .api_keys
        .iter()
        .any(|k| k.name == "owner:boss"));

    let (status, v) = op(
        &gw,
        "key_create",
        json!({ "name": "boss", "kind": "owner", "scope_mode": "all", "scope_patterns": "" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
}
