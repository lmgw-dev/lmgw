//! The key's policy on a conversion's page calls (document-conversion
//! design §7.5, D6; WP6 review findings S2, S6 and nits 1 and 2), against
//! `documents_model_help.rs`' mock model: the tokens-per-minute window, a
//! budget crossed in the middle of a request, a key disabled or expired
//! while its pages wait, a route without a price for a budgeted key, the
//! gateway's own budget, and the row of a caller without a key.

use std::time::Duration;

use lmgw_core::config::{hash_api_key, KeyPolicy, PriceScope, PriceTier, PriceUnit};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common::{serve, Gw};
use crate::documents_model_help::{
    codes, has_poppler, page_rows_of, request, settings, state_with, Model, ALIAS, PATH,
};

const KEY: &str = "lmgw-example-key";

/// Auth on, and the key [`KEY`] ("example-agent") with `policy`.
async fn with_key(state: &SharedState, policy: KeyPolicy) {
    settings(state, |s| s.auth_enabled = true).await;
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
             budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit, expires_at)
         VALUES ('example-agent', ?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(hash_api_key(KEY))
    .bind(policy.scope_mode.as_str())
    .bind(&policy.scope_patterns)
    .bind(policy.budget_micro)
    .bind(policy.budget_period.as_str())
    .bind(policy.rpm_limit)
    .bind(policy.tpm_limit)
    .bind(policy.concurrency_limit)
    .bind(policy.expires_at.clone())
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// The mock model's alias priced per token: a call of 1200 in and 300 out
/// costs 3600 + 4500 = 8100 micro-units.
async fn priced(state: &SharedState) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        ALIAS,
        PriceUnit::PerMtok,
        PriceTier::Standard,
        &Prices {
            price_in: Some(3.0),
            price_out: Some(15.0),
            source: PriceSource::Manual,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn convert_as_key(gw: &Gw, body: &Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{gw}{PATH}"))
        .bearer_auth(KEY)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

fn pages_of(doc: &Value, code: &str) -> Vec<u64> {
    codes(doc)
        .into_iter()
        .filter(|(c, _)| c == code)
        .flat_map(|(_, parts)| parts)
        .collect()
}

/// S2: a key's tokens-per-minute window bounds its conversion's pages. The
/// first call's tokens fill it; the pages left are not sent.
#[tokio::test]
async fn a_key_s_tokens_per_minute_window_stops_the_pages_left() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let state = state_with(&m, true).await;
    // One call in flight, so each page sees the tokens of the one before.
    settings(&state, |s| s.documents.tool_slots = 1).await;
    with_key(
        &state,
        KeyPolicy {
            tpm_limit: 1000,
            // The conversion is the one request the key makes: its pages
            // are not counted as requests.
            rpm_limit: 1,
            ..Default::default()
        },
    )
    .await;
    let gw = serve(state.clone()).await;
    let body = request(&[Some("Page one"), None, None, None], json!({}));
    let (status, a) = convert_as_key(&gw, &body).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    // The first scan page was read (1500 tokens, over the 1000 a minute);
    // the two after it were refused before anything was sent.
    assert_eq!(m.bodies().await.len(), 1);
    assert_eq!(d["stats"]["model_calls"], 1);
    assert_eq!(pages_of(d, "model_not_allowed"), [3, 4], "{d}");
    let message = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == "model_not_allowed")
        .and_then(|w| w["message"].as_str())
        .unwrap();
    assert!(message.contains("tokens/minute limit of 1000"), "{message}");
    assert_eq!(d["stats"]["parts_by_source"]["vision"], 1);
}

/// S2: a budget crossed in the middle of a request stops the pages left;
/// what it can be passed by is the calls in flight, no more.
#[tokio::test]
async fn a_budget_crossed_mid_request_stops_the_pages_left_within_the_bound() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::from_millis(300)).await;
    let state = state_with(&m, true).await;
    priced(&state).await;
    // Two calls in flight at once.
    settings(&state, |s| s.documents.tool_slots = 2).await;
    with_key(
        &state,
        KeyPolicy {
            // Less than one call costs.
            budget_micro: 5_000,
            ..Default::default()
        },
    )
    .await;
    let gw = serve(state.clone()).await;
    let body = request(&[None, None, None, None, None, None], json!({}));
    let (status, a) = convert_as_key(&gw, &body).await;
    let d = &a["documents"][0];
    assert_eq!(status, 200, "{a}");
    let sent = m.bodies().await.len();
    // Both calls in flight when the budget was crossed were sent; no more.
    assert!((1..=2).contains(&sent), "{sent} calls for a bound of 2");
    assert_eq!(d["stats"]["model_calls"], sent);
    assert_eq!(d["stats"]["models"][0]["in_flight_bound"], 2);
    let refused = pages_of(d, "model_not_allowed");
    assert_eq!(refused.len(), 6 - sent, "{d}");
    let message = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == "model_not_allowed")
        .and_then(|w| w["message"].as_str())
        .unwrap();
    assert!(message.contains("budget"), "{message}");
    // The spend the rows record is the calls that were sent.
    let rows = page_rows_of(&state, ALIAS, sent).await;
    assert_eq!(rows.len(), sent, "refused pages write no row of their own");
    assert!(rows.iter().all(|r| r.cost_micro == Some(8_100)), "{rows:?}");
}

/// S2: a key disabled, or past its expiry, while its pages wait stops
/// reading pages at its next call.
#[tokio::test]
async fn a_key_disabled_or_expired_mid_request_reads_no_further_page() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    for change in [
        "UPDATE api_keys SET enabled = 0 WHERE name = 'example-agent'",
        "UPDATE api_keys SET expires_at = '2001-01-01' WHERE name = 'example-agent'",
    ] {
        let m = Model::start(Duration::from_millis(1200)).await;
        let state = state_with(&m, true).await;
        settings(&state, |s| s.documents.tool_slots = 1).await;
        with_key(&state, KeyPolicy::default()).await;
        let gw = serve(state.clone()).await;
        let body = request(&[Some("Page one"), None, None, None], json!({}));
        let running = {
            let gw = gw.clone();
            tokio::spawn(async move { convert_as_key(&gw, &body).await })
        };
        // The first page is with the model; the key changes meanwhile.
        for _ in 0..1000 {
            if !m.arrivals.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sqlx::query(change).execute(&state.db).await.unwrap();
        state.reload_snapshot().await.unwrap();
        let (status, a) = running.await.unwrap();
        assert_eq!(status, 200, "{change}: {a}");
        let d = &a["documents"][0];
        assert_eq!(m.bodies().await.len(), 1, "{change}");
        assert_eq!(pages_of(d, "model_not_allowed"), [3, 4], "{change}: {d}");
        assert_eq!(d["stats"]["parts_by_source"]["vision"], 1, "{change}");
    }
}

/// Review nit 1 (and S6): a route that prices nothing a budgeted key is
/// billed in is refused once, where the route is opened: no page is
/// rendered, no call is sent, no row per page; a scan alone answers 403.
#[tokio::test]
async fn a_budgeted_key_on_an_unpriced_route_is_refused_once_and_a_scan_is_a_403() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let state = state_with(&m, true).await;
    with_key(
        &state,
        KeyPolicy {
            budget_micro: 1_000_000,
            ..Default::default()
        },
    )
    .await;
    let gw = serve(state.clone()).await;
    let body = request(&[Some("Page one"), None, None], json!({}));
    let (status, a) = convert_as_key(&gw, &body).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["content"], "Page one");
    assert_eq!(pages_of(d, "model_not_allowed"), [2, 3], "{d}");
    assert!(pages_of(d, "part_failed").is_empty(), "{d}");
    assert!(m.bodies().await.is_empty());
    assert_eq!(d["stats"]["model_calls"], 0);

    let (status, a) = convert_as_key(&gw, &request(&[None], json!({}))).await;
    let d = &a["documents"][0];
    assert_eq!(
        (status, &d["error"]["code"]),
        (403, &json!("model_not_allowed")),
        "{a}"
    );
    assert!(m.bodies().await.is_empty());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        page_rows_of(&state, ALIAS, 0).await.is_empty(),
        "a page that is never sent writes no row"
    );
}

/// The gateway's own budget applies to a caller without a key, and such a
/// caller's page call is a row all the same.
#[tokio::test]
async fn a_keyless_caller_s_call_is_a_row_and_meets_the_gateway_s_budget() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let state = state_with(&m, true).await;
    priced(&state).await;
    let gw = serve(state.clone()).await;
    let keyless = |body: Value| {
        let gw = gw.clone();
        async move {
            let r = reqwest::Client::new()
                .post(format!("{gw}{PATH}"))
                .json(&body)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    // Auth is off: no key, and the page is read.
    let (status, a) = keyless(request(&[Some("Page one"), None], json!({}))).await;
    assert_eq!(status, 200, "{a}");
    assert_eq!(a["documents"][0]["stats"]["parts_by_source"]["vision"], 1);
    let rows = page_rows_of(&state, ALIAS, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].status, rows[0].prompt_tokens), (200, Some(1200)));
    assert_eq!(rows[0].key_id, None, "no key is charged for a keyless call");
    assert_eq!(rows[0].cost_micro, Some(8_100), "and it is still priced");

    // The gateway's budget is spent: the next page is not sent.
    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, cost_micro)
         VALUES (strftime('%Y-%m-%dT%H','now'), 0, 'a', 1, 'chat', 'ok', 1, 2000000)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    settings(&state, |s| s.global_budget_micro = 1_000_000).await;
    let (status, a) = keyless(request(&[Some("Page one"), None], json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(pages_of(d, "model_not_allowed"), [2], "{d}");
    let message = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == "model_not_allowed")
        .and_then(|w| w["message"].as_str())
        .unwrap();
    assert!(message.contains("this gateway"), "{d}");
    assert_eq!(
        m.bodies().await.len(),
        1,
        "no call after the budget was spent"
    );
}
