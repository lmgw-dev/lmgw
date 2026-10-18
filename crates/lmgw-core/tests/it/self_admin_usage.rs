//! The answers of the price and usage tools that no OpenAPI document
//! describes (`lmgw__prices`, `lmgw__usage`), and the text of the three price
//! writes that the document does (`lmgw__price_set`, `lmgw__price_delete`,
//! `lmgw__prices_sync`): each tool's text is read into its type and written
//! out again, and the two texts are equal byte for byte. The text is the
//! answer's pretty JSON with sorted keys, so a key a type adds, drops, renames
//! or turns from absent into `null` (or an integer into a float) shows up
//! here.
//!
//! Every `/mcp/admin` call logs itself as a request of class `tool`, so a
//! usage window that is read twice sees the first call: the tests that count
//! name another `class`.

use lmgw_api_types as dto;
use lmgw_api_types::self_admin::{Prices, Usage};
use lmgw_core::config::{PriceScope, PriceUnit, Protocol, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices as Rates};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};

use crate::support::self_admin::{parse, read_back, Admin};

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

const SHEET_KEYS: [&str; 12] = [
    "id",
    "scope_kind",
    "scope_key",
    "unit",
    "price_in",
    "price_out",
    "price_cache_read",
    "price_cache_write",
    "price",
    "source",
    "note",
    "updated_at",
];

const CELL_KEYS: [&str; 22] = [
    "series",
    "requests",
    "tokens_in",
    "tokens_out",
    "tokens_cached",
    "tokens_cache_write",
    "tokens_reasoning",
    "audio_in_ms",
    "chars_in",
    "images_out",
    "cost",
    "cost_micro",
    "unpriced_requests",
    "unpriced_tokens",
    "unpriced_audio_in_ms",
    "unpriced_chars_in",
    "unpriced_images_out",
    "errors",
    "refusals",
    "avg_ttfb_ms",
    "avg_total_ms",
    "decode_tok_s",
];

async fn upstream(state: &SharedState, name: &str, expose_all: bool, prefix: &str) -> i64 {
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: name.into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            // Nothing in these tests asks it: the worklist reads the
            // snapshot and the rollup.
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all,
            expose_prefix: prefix.into(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    id
}

async fn alias(state: &SharedState, up: i64, model: &str, name: &str) {
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: name.into(),
            upstream_id: up,
            upstream_model_id: model.into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn sheet(
    state: &SharedState,
    (kind, key, unit): (PriceScope, &str, PriceUnit),
    rates: Rates,
    price: Option<f64>,
    note: Option<&str>,
) {
    store::upsert_price(&state.db, kind, key, unit, &rates, price, note)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// One rollup row. The columns not named are 0.
#[derive(Default)]
struct Cell {
    requests: i64,
    tokens_in: i64,
    tokens_out: i64,
    cost_micro: i64,
    cost_unknown_requests: i64,
    cost_unknown_tokens: i64,
    ttfb_sum: i64,
    ttfb_count: i64,
    total_sum: i64,
    total_count: i64,
    decode_tokens: i64,
    decode_ms: f64,
    audio_in_ms: i64,
    chars_in: i64,
    images_out: i64,
    cost_unknown_audio_in_ms: i64,
    cost_unknown_chars_in: i64,
    cost_unknown_images_out: i64,
}

async fn roll(
    state: &SharedState,
    (key_id, alias, upstream_id, class, outcome): (i64, &str, i64, &str, &str),
    c: Cell,
) {
    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, tokens_in, tokens_out, cost_micro, cost_unknown_requests,
             cost_unknown_tokens, ttfb_sum, ttfb_count, total_sum, total_count,
             decode_tokens, decode_ms, audio_in_ms, chars_in, images_out,
             cost_unknown_audio_in_ms, cost_unknown_chars_in, cost_unknown_images_out)
         VALUES (strftime('%Y-%m-%dT%H', 'now'), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
             ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
    )
    .bind(key_id)
    .bind(alias)
    .bind(upstream_id)
    .bind(class)
    .bind(outcome)
    .bind(c.requests)
    .bind(c.tokens_in)
    .bind(c.tokens_out)
    .bind(c.cost_micro)
    .bind(c.cost_unknown_requests)
    .bind(c.cost_unknown_tokens)
    .bind(c.ttfb_sum)
    .bind(c.ttfb_count)
    .bind(c.total_sum)
    .bind(c.total_count)
    .bind(c.decode_tokens)
    .bind(c.decode_ms)
    .bind(c.audio_in_ms)
    .bind(c.chars_in)
    .bind(c.images_out)
    .bind(c.cost_unknown_audio_in_ms)
    .bind(c.cost_unknown_chars_in)
    .bind(c.cost_unknown_images_out)
    .execute(&state.db)
    .await
    .unwrap();
}

#[tokio::test]
async fn the_prices_text_of_an_empty_gateway_is_four_keys_and_two_empty_lists() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;

    let text = admin.text("lmgw__prices", json!({})).await;
    let live = parse(&text);
    assert_keys(&live, &["currency", "sheets", "unpriced_models", "hint"]);
    assert_eq!(live["sheets"], json!([]));
    assert_eq!(live["unpriced_models"], json!([]));
    assert!(live["hint"].as_str().unwrap().contains("lmgw__price_set"));
    let prices: Prices = read_back(&text);
    assert_eq!(prices.currency, state.snapshot().settings.currency);
}

#[tokio::test]
async fn the_prices_text_lists_every_unit_and_the_models_nothing_prices() {
    let state = AppState::init_for_tests().await.unwrap();
    let cloud = upstream(&state, "cloud", false, "").await;
    alias(&state, cloud, "gpt-x", "chat-a").await;
    alias(&state, cloud, "gpt-y", "chat-b").await;
    let pass = upstream(&state, "kilo-gw", true, "kilo").await;
    // Passthrough models the rollup has seen, a name that routes nowhere (a
    // tool call), and a configured alias that was called too.
    for (name, requests) in [
        ("kilo/google/gemma", 206),
        ("lmgw__status", 17),
        ("kilo/deepseek/flash", 14),
        ("chat-a", 5),
    ] {
        roll(
            &state,
            (0, name, pass, "chat", "ok"),
            Cell {
                requests,
                ..Default::default()
            },
        )
        .await;
    }
    // chat-a: tokens and a fee, manual; chat-b: audio, characters and images;
    // a catalog row of the passthrough upstream, with no note.
    let tokens = Rates {
        price_in: Some(3.0),
        price_out: Some(15.0),
        price_cache_read: Some(0.3),
        price_cache_write: Some(3.75),
        source: PriceSource::Manual,
    };
    let unit_only = Rates {
        source: PriceSource::Manual,
        ..Default::default()
    };
    sheet(
        &state,
        (PriceScope::Alias, "chat-a", PriceUnit::PerMtok),
        tokens,
        None,
        Some("the owner's rate"),
    )
    .await;
    for (unit, price) in [
        (PriceUnit::PerAudioMinute, 6.0),
        (PriceUnit::PerMchar, 15.5),
        (PriceUnit::PerImage, 0.04),
    ] {
        sheet(
            &state,
            (PriceScope::Alias, "chat-b", unit),
            unit_only,
            Some(price),
            None,
        )
        .await;
    }
    sheet(
        &state,
        (
            PriceScope::UpstreamModel,
            &format!("{pass}:google/gemma"),
            PriceUnit::PerMtok,
        ),
        Rates {
            price_in: Some(1.0),
            price_out: Some(2.0),
            source: PriceSource::Catalog,
            ..Default::default()
        },
        None,
        None,
    )
    .await;

    let admin = Admin::new(&state).await;
    let text = admin.text("lmgw__prices", json!({})).await;
    let live = parse(&text);
    let prices: Prices = read_back(&text);

    // A sheet is the dashboard's own row, null where its unit has no rate.
    let sheets = live["sheets"].as_array().unwrap();
    assert_eq!(sheets.len(), 5);
    for s in sheets {
        assert_keys(s, &SHEET_KEYS);
    }
    let tokens_row = sheets.iter().find(|s| s["scope_key"] == "chat-a").unwrap();
    assert_eq!(tokens_row["unit"], "per_mtok");
    assert!(tokens_row["price_in"].is_f64() && tokens_row["price_in"] == 3.0);
    assert!(tokens_row["price"].is_null());
    assert_eq!(tokens_row["note"], "the owner's rate");
    let image_row = sheets.iter().find(|s| s["unit"] == "per_image").unwrap();
    assert!(image_row["price"].is_f64() && image_row["price"] == 0.04);
    assert!(image_row["price_in"].is_null() && image_row["note"].is_null());
    assert!(sheets
        .iter()
        .any(|s| s["source"] == "catalog" && s["scope_kind"] == "upstream_model"));
    assert_eq!(prices.sheets.len(), 5);

    // The worklist: both passthrough models, busiest first; chat-a and chat-b
    // are priced (a unit-only sheet is a price), the tool name routes nowhere.
    let unpriced = live["unpriced_models"].as_array().unwrap();
    for u in unpriced {
        assert_keys(u, &["name", "requests"]);
        assert!(u["requests"].is_i64(), "{u}");
    }
    assert_eq!(
        unpriced
            .iter()
            .map(|u| u["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["kilo/deepseek/flash"],
        "gemma is priced by its catalog row"
    );
    assert_eq!(prices.unpriced_models[0].requests, 14);
}

#[tokio::test]
async fn the_usage_text_of_a_window_with_nothing_in_it_has_every_key_and_nulls() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;

    // Nothing is of class image.
    let text = admin.text("lmgw__usage", json!({"class": "image"})).await;
    let live = parse(&text);
    assert_keys(
        &live,
        &[
            "from",
            "to",
            "class",
            "timezone",
            "currency",
            "units_since",
            "totals",
            "group_by",
            "breakdown",
            "hint",
        ],
    );
    assert_eq!(live["timezone"], "UTC");
    assert_eq!(live["group_by"], "alias");
    // The applied class filter is echoed; no alias was named, so no `alias`.
    assert_eq!(live["class"], "image");
    assert_eq!(live["breakdown"], json!([]));
    assert!(live["units_since"].is_string(), "{live}");
    let totals = &live["totals"];
    assert_keys(totals, &CELL_KEYS);
    assert!(totals["series"].is_null());
    assert_eq!(
        totals["cost"],
        format!("0.00 {}", live["currency"].as_str().unwrap())
    );
    for avg in ["avg_ttfb_ms", "avg_total_ms", "decode_tok_s"] {
        assert!(totals[avg].is_null(), "{avg}: {totals}");
    }
    assert_eq!(totals["requests"], 0);
    let usage: Usage = read_back(&text);
    assert_eq!(usage.totals.series, None);
}

#[tokio::test]
async fn the_usage_text_reads_back_equal_for_every_grouping_and_keeps_its_numbers() {
    let state = AppState::init_for_tests().await.unwrap();
    // Two cloud aliases, one priced with a remainder, one local with decode
    // speed; a long tail so the default limit folds an `Other`.
    roll(
        &state,
        (1, "gpt-5", 1, "chat", "ok"),
        Cell {
            requests: 10,
            tokens_in: 1000,
            tokens_out: 500,
            cost_micro: 12_840_000,
            ttfb_sum: 300,
            ttfb_count: 3,
            total_sum: 1000,
            total_count: 3,
            ..Default::default()
        },
    )
    .await;
    roll(
        &state,
        (1, "gpt-5", 1, "chat", "upstream_error"),
        Cell {
            requests: 2,
            ..Default::default()
        },
    )
    .await;
    roll(
        &state,
        (2, "whisper-1", 3, "chat", "refused"),
        Cell {
            requests: 1,
            ..Default::default()
        },
    )
    .await;
    roll(
        &state,
        (2, "whisper-1", 3, "chat", "ok"),
        Cell {
            requests: 4,
            audio_in_ms: 270_000,
            cost_micro: 1_620_000,
            cost_unknown_requests: 2,
            cost_unknown_audio_in_ms: 90_000,
            cost_unknown_chars_in: 4000,
            cost_unknown_images_out: 1,
            ..Default::default()
        },
    )
    .await;
    roll(
        &state,
        (0, "local-llama", 4, "chat", "ok"),
        Cell {
            requests: 6,
            decode_tokens: 1000,
            decode_ms: 2000.0,
            cost_unknown_requests: 1,
            cost_unknown_tokens: 40,
            ..Default::default()
        },
    )
    .await;
    for i in 1..=12 {
        roll(
            &state,
            (10 + i, &format!("m{i:02}"), 20 + i, "chat", "ok"),
            Cell {
                requests: i,
                cost_micro: i * 1000,
                ..Default::default()
            },
        )
        .await;
    }
    let admin = Admin::new(&state).await;

    // Every grouping, and the spellings the call echoes as given.
    for group in ["alias", "key", "class", "upstream", "none"] {
        let text = admin
            .text("lmgw__usage", json!({"group_by": group, "class": "chat"}))
            .await;
        let usage: Usage = read_back(&text);
        assert_eq!(usage.group_by, group);
        assert_eq!(usage.breakdown.is_empty(), group == "none");
    }
    let padded = admin
        .text("lmgw__usage", json!({"group_by": " key ", "class": "chat"}))
        .await;
    let usage: Usage = read_back(&padded);
    assert_eq!(usage.group_by, "key", "echoed as applied, trimmed");
    assert!(!usage.breakdown.is_empty());

    // The window: an inclusive date in, ISO 8601 UTC instants out, `to`
    // exclusive (the start of the day after the date asked for). The filters
    // are echoed as applied, trimmed.
    let text = admin
        .text(
            "lmgw__usage",
            json!({"from": "2026-10-01", "to": "2026-10-11", "alias": " a1 ", "class": "chat"}),
        )
        .await;
    let usage: Usage = read_back(&text);
    assert_eq!(usage.from, "2026-10-01T00:00:00Z");
    assert_eq!(usage.to, "2026-10-12T00:00:00Z");
    assert_eq!(usage.class.as_deref(), Some("chat"));
    assert_eq!(usage.alias.as_deref(), Some("a1"));
    // `<N>d` is N whole days ending with `to`; the default window is `30d`.
    let text = admin
        .text("lmgw__usage", json!({"from": "7d", "to": "2026-10-11"}))
        .await;
    let usage: Usage = read_back(&text);
    assert_eq!(usage.from, "2026-10-05T00:00:00Z");
    assert_eq!(usage.to, "2026-10-12T00:00:00Z");
    let text = admin.text("lmgw__usage", json!({"to": "2026-10-11"})).await;
    let usage: Usage = read_back(&text);
    assert_eq!(usage.from, "2026-09-12T00:00:00Z");
    assert_eq!(usage.to, "2026-10-12T00:00:00Z");
    let live = parse(&text);
    assert!(
        live.get("class").is_none() && live.get("alias").is_none(),
        "{live}"
    );

    // The default limit of 10 folds the tail into `Other`: ten series and the
    // fold. Every series, with the limit at its largest, is fifteen.
    let text = admin.text("lmgw__usage", json!({"class": "chat"})).await;
    let live = parse(&text);
    let _: Usage = read_back(&text);
    let rows = live["breakdown"].as_array().unwrap();
    assert_eq!(rows.len(), 11);
    for r in rows {
        assert_keys(r, &CELL_KEYS);
    }
    assert_eq!(rows[10]["series"], "Other");
    assert_eq!(rows[0]["series"], "gpt-5", "the dearest series is first");

    let text = admin
        .text("lmgw__usage", json!({"class": "chat", "limit": 100}))
        .await;
    let live = parse(&text);
    let usage: Usage = read_back(&text);
    let rows = live["breakdown"].as_array().unwrap();
    assert_eq!(rows.len(), 15);
    assert!(rows.iter().all(|r| r["series"] != "Other"));
    assert_eq!(usage.breakdown[0].series.as_deref(), Some("gpt-5"));

    // The numbers: money both ways, the remainder said inline, averages as
    // floats (whole ones included), integers as integers.
    let totals = &live["totals"];
    assert_eq!(totals["requests"], 10 + 2 + 1 + 4 + 6 + 78);
    let gpt = rows.iter().find(|r| r["series"] == "gpt-5").unwrap();
    assert_eq!(gpt["cost_micro"], 12_840_000);
    assert!(gpt["cost_micro"].is_i64() && gpt["requests"].is_i64());
    assert!(gpt["cost"].as_str().unwrap().starts_with("12.84 "), "{gpt}");
    assert!(!gpt["cost"].as_str().unwrap().contains("unpriced"), "{gpt}");
    assert!(
        gpt["avg_ttfb_ms"].is_f64() && gpt["avg_ttfb_ms"] == 100.0,
        "{gpt}"
    );
    assert!(gpt["avg_total_ms"].is_f64(), "{gpt}");
    assert!(gpt["decode_tok_s"].is_null());
    assert_eq!(
        (gpt["errors"].as_i64(), gpt["refusals"].as_i64()),
        (Some(2), Some(0))
    );
    let whisper = rows.iter().find(|r| r["series"] == "whisper-1").unwrap();
    assert!(
        whisper["cost"]
            .as_str()
            .unwrap()
            .ends_with("2 unpriced requests (0 tokens, 1.5 min audio, 4000 chars, 1 image)"),
        "{whisper}"
    );
    assert_eq!(whisper["unpriced_audio_in_ms"], 90_000);
    assert_eq!(whisper["refusals"], 1);
    let llama = rows.iter().find(|r| r["series"] == "local-llama").unwrap();
    assert!(
        llama["decode_tok_s"].is_f64() && llama["decode_tok_s"] == 500.0,
        "{llama}"
    );
    assert!(llama["cost"]
        .as_str()
        .unwrap()
        .contains("1 unpriced request (40 tokens)"));
    assert!(llama["avg_ttfb_ms"].is_null());
}

#[tokio::test]
async fn the_usage_refusals_name_the_argument_and_the_spellings() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    for (args, needle) in [
        (json!({"group_by": "tenant"}), "unknown group_by 'tenant'"),
        (json!({"class": "video"}), "unknown class 'video'"),
        (json!({"from": "yesterday"}), "invalid 'from' date"),
        (json!({"from": "999999999d"}), "reaches before the calendar"),
        (json!({"from": "0d"}), "at least 1"),
        (json!({"to": "tomorrow"}), "invalid 'to' date"),
        (
            json!({"from": "2026-10-11", "to": "2026-10-06"}),
            "is after 'to'",
        ),
    ] {
        let (failed, text) = admin.call("lmgw__usage", args.clone()).await;
        assert!(failed && text.contains(needle), "{args}: {text}");
    }
}

#[tokio::test]
async fn the_price_write_tools_answer_the_types_their_http_ops_do() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;

    let text = admin
        .text(
            "lmgw__price_set",
            json!({"scope_kind": "alias", "scope_key": "t1", "price_in": 3, "price_out": 15}),
        )
        .await;
    assert_keys(
        &parse(&text),
        &["ok", "scope_kind", "scope_key", "unit", "source"],
    );
    let saved: dto::PriceSaved = read_back(&text);
    assert_eq!(saved.unit, "per_mtok");
    assert_eq!(
        (saved.scope_kind.as_str(), saved.source.as_str()),
        ("alias", "manual")
    );

    let text = admin
        .text(
            "lmgw__price_set",
            json!({"scope_kind": "upstream_model", "scope_key": " 1:foo ", "unit": "per_request", "price": 0.002}),
        )
        .await;
    let saved: dto::PriceSaved = read_back(&text);
    assert_eq!(
        (
            saved.scope_kind.as_str(),
            saved.scope_key.as_str(),
            saved.unit.as_str()
        ),
        ("upstream_model", "1:foo", "per_request")
    );

    let id = store::list_prices(&state.db).await.unwrap()[0].id;
    let text = admin.text("lmgw__price_delete", json!({"id": id})).await;
    assert_keys(&parse(&text), &["ok", "id"]);
    let ack: dto::IdAck = read_back(&text);
    assert_eq!(ack.id, id);

    // An id no row has is a refusal naming it, not an ack for nothing.
    let (failed, text) = admin.call("lmgw__price_delete", json!({"id": id})).await;
    assert!(
        failed && text.contains(&format!("no price row {id}")),
        "{text}"
    );

    // Nobody to ask: the empty summary.
    let text = admin.text("lmgw__prices_sync", json!({})).await;
    let summary: dto::PriceSyncSummary = read_back(&text);
    assert_eq!(summary, dto::PriceSyncSummary::default());
}
