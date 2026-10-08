//! Billable units on the surfaces (billable-units design §8, §10): a
//! request fee reaches the usage plane, the CSV and `lmgw__usage` as money
//! beside the tokens; a stream stopped before its answer on a fee-priced
//! scope is unpriced — never the fee, never 0 — and lands in the remainder;
//! `price_set` checks each unit's shape on the op and on the tool alike; the
//! CSV carries the six quantity columns at its end; and `lmgw__usage` names
//! the quantities its remainder holds.

use std::time::Duration;

use lmgw_core::config::{PriceScope, PriceUnit, Protocol, SelfAdmin, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Map, Value};
use tokio::sync::Notify;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};
use crate::e2e_proxy::setup_kind;
use crate::stream_usage::row_after;
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, send, text_session, user_text, Turn,
};

async fn op(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn get_json(gw: &Gw, route: &str) -> Value {
    let resp = gw
        .client()
        .get(format!("{gw}{route}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{route}");
    resp.json().await.unwrap()
}

/// A self-admin tool as the owner, at full self-admin: its text and
/// whether it is an error result.
async fn tool(state: &SharedState, name: &str, args: Value) -> (String, bool) {
    let mut settings = state.snapshot().settings.clone();
    if settings.self_admin != SelfAdmin::Full {
        settings.self_admin = SelfAdmin::Full;
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();
    }
    let args: Option<Map<String, Value>> = args.as_object().cloned();
    let result = lmgw_core::mcp::selfadmin::call(state, name, args)
        .await
        .unwrap();
    (
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        result["isError"].as_bool().unwrap_or(false),
    )
}

/// The rows of `prices` for one alias scope, as `(unit, source, price_in,
/// price)`.
async fn rows_of(
    state: &SharedState,
    alias: &str,
) -> Vec<(PriceUnit, PriceSource, Option<f64>, Option<f64>)> {
    store::list_prices(&state.db)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.scope_kind == PriceScope::Alias && r.scope_key == alias)
        .map(|r| (r.unit, r.source, r.price_in, r.price))
        .collect()
}

/// The current UTC hour key, which every default window holds.
fn this_hour() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H").to_string()
}

/// §3.3's mixed golden on the surfaces: 1M in and 100k out at 3/15 plus a
/// 0.005 fee set through the op is 4 505 000 micro on the series, the CSV
/// and the tool — the fee is money like the tokens, in the one total.
#[tokio::test]
async fn a_request_fee_reaches_the_usage_plane_the_csv_and_the_tool() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1_000_000, "completion_tokens": 100_000}
        })))
        .mount(&mock)
        .await;
    let (state, gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "my-model",
        PriceUnit::PerMtok,
        &Prices {
            price_in: Some(3.0),
            price_out: Some(15.0),
            source: PriceSource::Catalog,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    // The fee goes in through the dashboard's own op, in its unit.
    let (status, v) = op(
        &gw,
        "price_set",
        json!({"scope_key": "my-model", "unit": "per_request", "price": 0.005}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["unit"], "per_request", "{v}");

    let resp = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let row = row_after(&state, 0).await;
    assert_eq!(row.cost_micro, Some(4_505_000), "{row:?}");

    let series = get_json(&gw, "/api/usage/series?group_by=alias").await;
    assert_eq!(series["totals"]["cost_micro"], 4_505_000, "{series}");
    assert_eq!(series["totals"]["cost_unknown_requests"], 0, "{series}");
    assert!(
        series["units_since"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the series says when quantities started: {series}"
    );

    let (text, is_error) = tool(&state, "lmgw__usage", json!({})).await;
    assert!(!is_error, "{text}");
    let usage: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(usage["totals"]["cost_micro"], 4_505_000, "{usage}");
    assert_eq!(usage["breakdown"][0]["series"], "my-model", "{usage}");
    assert!(usage["units_since"].is_string(), "{usage}");

    // The tool lists the fee row with its unit and rate beside the token row.
    let (text, is_error) = tool(&state, "lmgw__prices", json!({})).await;
    assert!(!is_error, "{text}");
    let prices: Value = serde_json::from_str(&text).unwrap();
    let fee = prices["sheets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["unit"] == "per_request")
        .unwrap_or_else(|| panic!("no per_request sheet: {prices}"));
    assert_eq!(fee["price"], 0.005, "{fee}");
    assert_eq!(fee["price_in"], Value::Null, "{fee}");
    assert_eq!(fee["source"], "manual", "{fee}");
    assert!(
        prices["hint"]
            .as_str()
            .unwrap()
            .contains("per_audio_minute per minute of input audio"),
        "{prices}"
    );
}

/// A realtime response canceled while the upstream still held its request
/// (§4.5): the upstream may or may not be working on it, so the request
/// count is unknown, and on a scope priced per request the row is unpriced
/// — not the fee, and not 0 — and is counted in the remainder the totals
/// state, with the rate it would have paid on the row.
#[tokio::test]
async fn a_stream_stopped_before_its_answer_on_a_fee_scope_is_unpriced_and_in_the_remainder() {
    let fake = chat_fake().await;
    let never = std::sync::Arc::new(Notify::new());
    fake.push(Turn::Held(never.clone(), Box::new(Turn::text(&["late"]))));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "chatty",
        PriceUnit::PerRequest,
        &Prices {
            source: PriceSource::Manual,
            ..Default::default()
        },
        Some(0.005),
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let mut ws = text_session(&addr).await;
    send(&mut ws, user_text("Erzähl was")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.created").await;
    for _ in 0..500 {
        if fake.seen.chat_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fake.seen.chat_count(),
        1,
        "the request is with the upstream"
    );
    send(&mut ws, json!({"type": "response.cancel"})).await;
    events_until(&mut ws, "response.done").await;

    let mut found = None;
    for _ in 0..500 {
        let rows = store::query_logs(&state.db, &Default::default())
            .await
            .unwrap();
        if let Some(r) = rows.into_iter().find(|r| r.requested_alias == "chatty") {
            found = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let row = found.expect("the canceled call wrote its row");
    assert_eq!(row.status, 200, "{row:?}");
    assert_eq!(row.error_kind.as_deref(), Some("canceled"), "{row:?}");
    assert_eq!(row.cost_micro, None, "unknown, not the fee: {row:?}");
    assert_eq!(row.cost_units_micro, None, "{row:?}");
    assert_eq!(
        row.price_per_request,
        Some(0.005),
        "the rate is still on the row"
    );

    let gw = serve(state.clone()).await;
    let top = get_json(&gw, "/api/usage/top?dim=alias").await;
    let chatty = top["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["series"] == "chatty")
        .unwrap_or_else(|| panic!("no chatty cell: {top}"));
    assert_eq!(chatty["cost_micro"], 0, "{chatty}");
    assert_eq!(
        chatty["cost_unknown_requests"], 1,
        "in the remainder: {chatty}"
    );
}

/// `price_set` checks a row's shape once, for both planes: the token unit
/// takes the four token rates and refuses `price`, every other unit takes
/// `price` and refuses the token rates, each refusal names the unit's
/// scale, and an unknown unit is refused by name. A row in another unit
/// sits beside the token row of the same scope rather than replacing it.
#[tokio::test]
async fn price_set_checks_each_unit_on_the_op_and_on_the_tool() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;

    // The op (the dashboard's plane).
    let refused = [
        (
            json!({"scope_key": "a", "unit": "per_mtok", "price": 1.0}),
            "per 1M tokens",
        ),
        (
            json!({"scope_key": "a", "unit": "per_audio_minute", "price_in": 1.0, "price": 0.006}),
            "per minute of input audio",
        ),
        (
            json!({"scope_key": "a", "unit": "per_image"}),
            "per generated image",
        ),
        (
            json!({"scope_key": "a", "unit": "per_second", "price": 1.0}),
            "unknown unit 'per_second'",
        ),
        (
            json!({"scope_key": "a", "unit": "per_request", "price": -1.0}),
            "price must be",
        ),
    ];
    for (body, says) in &refused {
        let (status, v) = op(&gw, "price_set", body.clone()).await;
        assert_ne!(status, 200, "{body} was accepted: {v}");
        assert!(
            v.to_string().contains(says),
            "{body}: {v} does not say '{says}'"
        );
    }
    assert!(
        rows_of(&state, "a").await.is_empty(),
        "nothing refused was written"
    );

    let (status, v) = op(
        &gw,
        "price_set",
        json!({"scope_key": "a", "price_in": 3.0, "price_out": 15.0}),
    )
    .await;
    assert_eq!(
        status, 200,
        "a call from before units still writes tokens: {v}"
    );
    let (status, v) = op(
        &gw,
        "price_set",
        json!({"id": 999, "scope_key": "a", "unit": "per_mchar", "price": 15.0}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        rows_of(&state, "a").await,
        vec![
            (PriceUnit::PerMchar, PriceSource::Manual, None, Some(15.0)),
            (PriceUnit::PerMtok, PriceSource::Manual, Some(3.0), None),
        ],
        "two rows of one scope, side by side; the posted id is ignored"
    );

    // The tool, with the same rules.
    let (text, is_error) = tool(
        &state,
        "lmgw__price_set",
        json!({"scope_kind": "alias", "scope_key": "b", "unit": "per_image", "price_out": 0.04}),
    )
    .await;
    assert!(is_error, "{text}");
    assert!(
        text.contains("per generated image") && text.contains("price_out"),
        "{text}"
    );
    let (text, is_error) = tool(
        &state,
        "lmgw__price_set",
        json!({"scope_kind": "alias", "scope_key": "b", "unit": "per_mtok", "price": 1.0}),
    )
    .await;
    assert!(is_error && text.contains("per 1M tokens"), "{text}");
    let (text, is_error) = tool(
        &state,
        "lmgw__price_set",
        json!({"scope_kind": "alias", "scope_key": "b", "unit": "per_image", "price": 0.04}),
    )
    .await;
    assert!(!is_error, "{text}");
    assert_eq!(
        rows_of(&state, "b").await,
        vec![(PriceUnit::PerImage, PriceSource::Manual, None, Some(0.04))]
    );
}

/// The CSV's six new columns are its last (§8.5), so a sheet that reads the
/// older columns by position still reads them; and `lmgw__usage` states
/// the remainder with the quantities it holds and names its measured
/// quantities beside the tokens (§8.3).
#[tokio::test]
async fn the_csv_appends_the_quantities_and_lmgw_usage_names_what_the_remainder_holds() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, alias, class, requests, cost_micro,
             audio_in_ms, chars_in, images_out,
             cost_unknown_requests, cost_unknown_tokens,
             cost_unknown_audio_in_ms, cost_unknown_chars_in, cost_unknown_images_out)
         VALUES (?1, 'whisper', 'audio', 5, 2700, 297000, 0, 0, 3, 0, 270000, 0, 0)",
    )
    .bind(this_hour())
    .execute(&state.db)
    .await
    .unwrap();
    let gw = serve(state.clone()).await;

    let resp = gw
        .client()
        .get(format!("{gw}/api/usage/export.csv"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let csv = resp.text().await.unwrap();
    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    assert_eq!(
        header[header.len() - 6..],
        [
            "audio_in_ms",
            "chars_in",
            "images_out",
            "cost_unknown_audio_in_ms",
            "cost_unknown_chars_in",
            "cost_unknown_images_out",
        ],
        "{csv}"
    );
    assert_eq!(
        header[..8],
        [
            "bucket_utc",
            "key_id",
            "key_name",
            "alias",
            "upstream_id",
            "upstream_name",
            "class",
            "outcome"
        ],
        "the older columns keep their places"
    );
    let row: Vec<&str> = lines.next().unwrap().split(',').collect();
    assert_eq!(row.len(), header.len(), "{csv}");
    assert_eq!(
        row[row.len() - 6..],
        ["297000", "0", "0", "270000", "0", "0"],
        "{csv}"
    );

    let (text, is_error) = tool(&state, "lmgw__usage", json!({})).await;
    assert!(!is_error, "{text}");
    let usage: Value = serde_json::from_str(&text).unwrap();
    let totals = &usage["totals"];
    let currency = usage["currency"].as_str().unwrap();
    assert_eq!(
        totals["cost"],
        format!("0.00 {currency} · 3 unpriced requests (0 tokens, 4.5 min audio)"),
        "{usage}"
    );
    assert_eq!(totals["audio_in_ms"], 297_000, "{totals}");
    assert_eq!(totals["unpriced_audio_in_ms"], 270_000, "{totals}");
    assert_eq!(totals["chars_in"], 0, "{totals}");
}

/// The folded "Other" series carries the quantities of what it folds (WP1
/// review): a tail beyond the limit still reads its measured minutes, and
/// its remainder's, instead of zeros.
#[tokio::test]
async fn the_other_series_carries_the_quantities_it_folds() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    for (alias, cost, audio, unknown_chars) in [
        ("big", 9_000, 0, 0),
        ("asr", 10, 60_000, 0),
        ("tts", 5, 0, 1234),
    ] {
        sqlx::query(
            "INSERT INTO usage_hourly (bucket_utc, alias, requests, cost_micro, audio_in_ms,
                 chars_in, cost_unknown_requests, cost_unknown_chars_in)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?5)",
        )
        .bind(this_hour())
        .bind(alias)
        .bind(cost)
        .bind(audio)
        .bind(unknown_chars)
        .bind((unknown_chars > 0) as i64)
        .execute(&state.db)
        .await
        .unwrap();
    }
    let gw = serve(state.clone()).await;
    let series = get_json(&gw, "/api/usage/series?group_by=alias&limit=1").await;
    let other: Vec<&Value> = series["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["series"] != "big")
        .collect();
    assert_eq!(other.len(), 1, "one folded cell: {series}");
    assert_eq!(other[0]["audio_in_ms"], 60_000, "{series}");
    assert_eq!(other[0]["chars_in"], 1234, "{series}");
    assert_eq!(other[0]["cost_unknown_chars_in"], 1234, "{series}");

    let (text, is_error) = tool(&state, "lmgw__usage", json!({"limit": 1})).await;
    assert!(!is_error, "{text}");
    let usage: Value = serde_json::from_str(&text).unwrap();
    let other = &usage["breakdown"][1];
    assert_eq!(other["series"], "Other", "{usage}");
    assert_eq!(other["audio_in_ms"], 60_000, "{usage}");
    assert_eq!(other["unpriced_chars_in"], 1234, "{usage}");
    assert!(
        other["cost"]
            .as_str()
            .unwrap()
            .ends_with("(0 tokens, 1234 chars)"),
        "{other}"
    );
}

/// A rate the op plane cannot read is refused by name (WP5 review): a "3" in
/// quotes used to be dropped, and the row written without the rate it was
/// meant to carry.
#[tokio::test]
async fn a_rate_that_is_no_number_is_refused_by_name() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (status, v) = op(
        &gw,
        "price_set",
        json!({"scope_key": "a", "price_in": "3", "price_out": 15.0}),
    )
    .await;
    assert_ne!(status, 200, "{v}");
    assert!(v.to_string().contains("price_in must be a number"), "{v}");
    assert!(rows_of(&state, "a").await.is_empty(), "nothing was written");
}

/// A row's detail says "unpriced" exactly where the Usage page counts it
/// (WP5 review F1): `/api/logs` carries the rollup's own rule. An answered
/// call, one stopped mid-way and a failure that spent tokens are in the
/// remainder; a GPU-hold refusal and a tool call are not.
#[tokio::test]
async fn a_log_row_says_whether_the_remainder_counts_it() {
    use lmgw_core::telemetry::RequestClass;
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let rows = [
        ("answered", 200, None, RequestClass::Chat, Some(12), true),
        (
            "stopped",
            200,
            Some("canceled"),
            RequestClass::Chat,
            None,
            true,
        ),
        (
            "failed-spent",
            502,
            Some("transport"),
            RequestClass::Chat,
            Some(40),
            true,
        ),
        (
            "held",
            503,
            Some("gpu_hold"),
            RequestClass::Chat,
            None,
            false,
        ),
        ("tool", 200, None, RequestClass::Tool, None, false),
    ];
    for (alias, status, kind, class, prompt, _) in &rows {
        store::insert_request_log(
            &state.db,
            &store::NewRequestLog {
                ingress_proto: "openai".into(),
                requested_alias: alias.to_string(),
                status: *status,
                error_kind: kind.map(str::to_string),
                class: *class,
                prompt_tokens: *prompt,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let gw = serve(state.clone()).await;
    let logs = get_json(&gw, "/api/logs?limit=50").await;
    for (alias, .., counted) in &rows {
        let row = logs["logs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["requested_alias"] == *alias)
            .unwrap_or_else(|| panic!("no {alias} row: {logs}"));
        assert_eq!(
            row["pricing"]["in_remainder"],
            json!(counted),
            "{alias}: {row}"
        );
    }
    let series = get_json(&gw, "/api/usage/series").await;
    assert_eq!(
        series["totals"]["cost_unknown_requests"],
        rows.iter().filter(|r| r.5).count(),
        "the rollup counts the same rows: {series}"
    );
}
