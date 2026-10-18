//! What the three price ops answer over HTTP (`price_set`, `price_delete`,
//! `prices_sync`): each answer reads back through the type the document
//! describes for it and keeps the keys it has always sent. Which values are
//! whole numbers, which are `null` and which are floats.
//!
//! The usage plane's own routes (`GET /api/usage/*`) are pinned elsewhere; the
//! tools `lmgw__prices` and `lmgw__usage` in `self_admin_usage.rs`.

use lmgw_api_types as dto;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, serve, Gw};

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

/// The keys of `live`, to be exactly `want` (in any order).
fn assert_keys(live: &Value, want: &[&str]) {
    let mut got: Vec<&str> = live
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {live}"))
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = want.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "{live}");
}

#[tokio::test]
async fn price_set_answers_the_scope_and_unit_it_wrote_in_every_unit() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;

    // (body, scope_kind, scope_key, unit): the default scope is an alias, the
    // key and the kind are trimmed, the default unit is tokens.
    let cases = [
        (
            json!({"scope_kind": "alias", "scope_key": "a1", "price_in": 3, "price_out": 15,
                   "price_cache_read": 0.3, "price_cache_write": 3.75, "note": "hello"}),
            ("alias", "a1", "per_mtok"),
        ),
        (
            json!({"scope_key": "  a2  ", "price_in": 0.5}),
            ("alias", "a2", "per_mtok"),
        ),
        (
            json!({"scope_kind": " upstream_model ", "scope_key": "1:m", "price_out": 2.5}),
            ("upstream_model", "1:m", "per_mtok"),
        ),
        (
            json!({"scope_key": "a3", "unit": "per_audio_minute", "price": 0.006}),
            ("alias", "a3", "per_audio_minute"),
        ),
        (
            json!({"scope_key": "a3", "unit": "per_mchar", "price": 15}),
            ("alias", "a3", "per_mchar"),
        ),
        (
            json!({"scope_key": "a3", "unit": "per_image", "price": 0.04}),
            ("alias", "a3", "per_image"),
        ),
        (
            json!({"scope_key": "a3", "unit": "per_request", "price": 0}),
            ("alias", "a3", "per_request"),
        ),
        (
            // The editor also posts the row's id; the op writes by scope and
            // unit and answers the same five keys.
            json!({"id": 7, "scope_key": "a1", "unit": " per_mtok ", "price_in": 4.0}),
            ("alias", "a1", "per_mtok"),
        ),
    ];
    for (body, (kind, key, unit)) in cases {
        let (status, live) = op(&gw, "price_set", body.clone()).await;
        assert_eq!(status, 200, "{body}: {live}");
        let saved = common::round_trips::<dto::PriceSaved>(&format!("price_set {body}"), &live);
        assert_keys(&live, &["ok", "scope_kind", "scope_key", "unit", "source"]);
        assert_eq!(
            saved,
            dto::PriceSaved {
                ok: true,
                scope_kind: kind.into(),
                scope_key: key.into(),
                unit: unit.into(),
                source: "manual".into(),
            },
            "{body}"
        );
    }
    // Seven distinct rows: a1 per_mtok was written twice.
    assert_eq!(store::list_prices(&state.db).await.unwrap().len(), 7);
}

#[tokio::test]
async fn price_set_refuses_with_the_flat_error_and_no_row() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    for (body, needle) in [
        (
            json!({"scope_kind": "nope", "scope_key": "a", "price_in": 1}),
            "unknown scope_kind 'nope'",
        ),
        (json!({"price_in": 1}), "scope_key is required"),
        (
            json!({"scope_key": "a", "unit": "per_second", "price": 1}),
            "unknown unit 'per_second'",
        ),
        (
            json!({"scope_key": "a", "price_in": "3"}),
            "price_in must be a number",
        ),
        (
            json!({"scope_key": "a", "unit": "per_image"}),
            "a per_image row needs price",
        ),
    ] {
        let (status, live) = op(&gw, "price_set", body.clone()).await;
        assert_eq!(status, 400, "{body}: {live}");
        assert_keys(&live, &["code", "message"]);
        assert_eq!(live["code"], "op_failed", "{live}");
        assert!(live["message"].as_str().unwrap().contains(needle), "{live}");
    }
    assert!(store::list_prices(&state.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn price_delete_answers_the_id_it_deleted_and_a_404_for_an_id_no_row_has() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (status, _) = op(
        &gw,
        "price_set",
        json!({"scope_key": "a", "price_in": 1, "price_out": 2}),
    )
    .await;
    assert_eq!(status, 200);
    let id = store::list_prices(&state.db).await.unwrap()[0].id;

    let (status, live) = op(&gw, "price_delete", json!({"id": id})).await;
    assert_eq!(status, 200, "{live}");
    let ack = common::round_trips::<dto::IdAck>("price_delete", &live);
    assert_keys(&live, &["ok", "id"]);
    assert!(live["id"].is_i64(), "{live}");
    assert_eq!(ack, dto::IdAck { ok: true, id });
    assert!(store::list_prices(&state.db).await.unwrap().is_empty());

    // The same id again, and one that never existed: nothing was deleted, and
    // the answer says so with the dashboard dialect's not-found.
    for given in [id, 99_999] {
        let (status, live) = op(&gw, "price_delete", json!({"id": given})).await;
        assert_eq!(status, 404, "{live}");
        assert_keys(&live, &["code", "message"]);
        assert_eq!(live["code"], "not_found", "{live}");
        assert!(
            live["message"]
                .as_str()
                .unwrap()
                .contains(&given.to_string()),
            "{live}"
        );
    }

    // A missing or non-integer id is the flat refusal, not an ack.
    for body in [json!({}), json!({"id": "3"}), json!({"id": 4.5})] {
        let (status, live) = op(&gw, "price_delete", body.clone()).await;
        assert_eq!(status, 400, "{body}: {live}");
        assert_eq!(live["message"], "pass id", "{live}");
    }
}

/// A line of `upstreams`: the totals' five keys, and which upstream and how it
/// went.
const UPSTREAM_LINE_KEYS: [&str; 7] = [
    "error",
    "not_synced",
    "rows_removed",
    "rows_written",
    "unpriced_models",
    "upstream",
    "upstream_id",
];

fn catalog(fee: Option<&str>) -> Value {
    let mut priced = json!({
        "prompt": "0.000001", "completion": "0.000002",
        "input_cache_read": "0.0000001", "web_search": "0.01", "image": "0.003",
    });
    if let Some(f) = fee {
        priced["request"] = json!(f);
    }
    json!({ "data": [
        { "id": "vendor/a", "pricing": priced },
        { "id": "vendor/b", "pricing": { "prompt": "0.000003", "completion": "0.000015" } },
        { "id": "vendor/odd", "pricing": { "prompt": "n/a", "completion": "0.000002" } },
        { "id": "vendor/none" },
    ]})
}

async fn mount_models(server: &MockServer, status: u16, body: Value) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn cloud_upstream(state: &SharedState, name: &str, server: &MockServer) -> i64 {
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: name.into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", server.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    id
}

#[tokio::test]
async fn prices_sync_answers_an_empty_summary_when_there_is_nobody_to_ask() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;
    let (status, live) = op(&gw, "prices_sync", json!({})).await;
    assert_eq!(status, 200, "{live}");
    let summary = common::round_trips::<dto::PriceSyncSummary>("prices_sync (no upstream)", &live);
    assert_eq!(summary, dto::PriceSyncSummary::default());
    assert_eq!(
        live,
        json!({"rows_written": 0, "rows_removed": 0, "unpriced_models": 0,
               "not_synced": {}, "upstreams": []})
    );
}

#[tokio::test]
async fn prices_sync_answers_the_totals_and_a_line_per_upstream() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let good = MockServer::start().await;
    mount_models(&good, 200, catalog(Some("0.002"))).await;
    let up = cloud_upstream(&state, "kilo", &good).await;
    for (model, alias) in [("vendor/a", "alias-a"), ("vendor/b", "alias-b")] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up,
                upstream_model_id: model.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();

    let (status, live) = op(&gw, "prices_sync", json!({})).await;
    assert_eq!(status, 200, "{live}");
    let first = common::round_trips::<dto::PriceSyncSummary>("prices_sync", &live);
    assert_keys(
        &live,
        &[
            "not_synced",
            "rows_removed",
            "rows_written",
            "unpriced_models",
            "upstreams",
        ],
    );
    // alias-a's tokens and its request fee, alias-b's tokens; the model with
    // a price that does not parse and the one with none are unpriced.
    assert_eq!(first.rows_written, 3);
    assert_eq!(first.unpriced_models, 2);
    assert_eq!(live["not_synced"], json!({"image": 1, "web_search": 1}));
    let line = &live["upstreams"][0];
    assert_keys(line, &UPSTREAM_LINE_KEYS);
    assert!(line["error"].is_null(), "{line}");
    assert!(
        line["upstream_id"].is_i64() && line["rows_written"].is_u64(),
        "{line}"
    );
    assert_eq!(first.upstreams[0].upstream, "kilo");
    assert_eq!(first.upstreams[0].not_synced["web_search"], 1);

    // The catalog stops publishing the fee: the next run removes its row and
    // says so, in the total and on the upstream's line.
    mount_models(&good, 200, catalog(None)).await;
    state.catalog.invalidate(up).await;
    let (status, live) = op(&gw, "prices_sync", json!({})).await;
    assert_eq!(status, 200, "{live}");
    let second = common::round_trips::<dto::PriceSyncSummary>("prices_sync (fee gone)", &live);
    assert_eq!(
        (second.rows_removed, second.upstreams[0].rows_removed),
        (1, 1)
    );

    // An upstream that cannot be asked is named, with the reason, and the run
    // goes on: its line is all zeros and `error` is text, not `null`.
    let bad = MockServer::start().await;
    mount_models(&bad, 500, json!({})).await;
    cloud_upstream(&state, "broken", &bad).await;
    state.catalog.invalidate(up).await;
    let (status, live) = op(&gw, "prices_sync", json!({})).await;
    assert_eq!(status, 200, "{live}");
    let third = common::round_trips::<dto::PriceSyncSummary>("prices_sync (one down)", &live);
    assert_eq!(third.upstreams.len(), 2);
    let broken = third
        .upstreams
        .iter()
        .find(|u| u.upstream == "broken")
        .unwrap();
    assert!(
        broken.error.as_deref().unwrap().contains("500"),
        "{broken:?}"
    );
    assert_eq!((broken.rows_written, broken.unpriced_models), (0, 0));
    assert!(broken.not_synced.is_empty());
    assert!(third.upstreams.iter().any(|u| u.error.is_none()));
}
