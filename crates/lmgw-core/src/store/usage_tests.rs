use sqlx::{Row, SqlitePool};

use crate::config::{PriceScope, Snapshot};
use crate::ir::Timings;
use crate::pricing::Cost;
use crate::telemetry::RequestClass;

use super::*;
use crate::pricing::{PriceSource, Prices, TokenUsage};

async fn db() -> SqlitePool {
    open_in_memory().await.unwrap()
}

fn log(alias: &str, prompt: i64, completion: i64, cost: Cost) -> NewRequestLog {
    NewRequestLog {
        ingress_proto: "openai".into(),
        requested_alias: alias.into(),
        upstream_id: Some(1),
        status: 200,
        ttfb_ms: Some(120),
        total_ms: Some(900),
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        cost,
        ..Default::default()
    }
}

fn priced(micro: i64) -> Cost {
    Cost {
        total_micro: Some(micro),
        in_micro: Some(micro),
        out_micro: Some(0),
        source: PriceSource::Catalog,
        used: Prices::default(),
    }
}

fn window() -> UsageFilter {
    UsageFilter {
        from: "2000-01-01T00".into(),
        to: "2999-01-01T00".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn rollup_totals_equal_a_direct_scan_of_the_raw_rows() {
    let pool = db().await;
    // A deterministic spread: three aliases, a few sizes, one unpriced.
    let mut expect_cost = 0i64;
    let mut expect_in = 0i64;
    for i in 0..40i64 {
        let alias = ["a", "b", "c"][(i % 3) as usize];
        let cost = if i % 7 == 0 {
            Cost::unknown()
        } else {
            expect_cost += i * 100;
            priced(i * 100)
        };
        expect_in += i * 10;
        insert_request_log(&pool, &log(alias, i * 10, i, cost))
            .await
            .unwrap();
    }

    let t = usage_totals(&pool, &window()).await.unwrap();
    assert_eq!(t.requests, 40);
    assert_eq!(t.cost_micro, expect_cost);
    assert_eq!(t.tokens_in, expect_in);

    // …and the same numbers straight off request_logs.
    let raw = sqlx::query(
        "SELECT COUNT(*) AS n, COALESCE(SUM(cost_micro),0) AS c,
                    COALESCE(SUM(prompt_tokens),0) AS p FROM request_logs",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(t.requests, raw.get::<i64, _>("n"));
    assert_eq!(t.cost_micro, raw.get::<i64, _>("c"));
    assert_eq!(t.tokens_in, raw.get::<i64, _>("p"));
}

#[tokio::test]
async fn an_unpriced_request_adds_nothing_to_cost_and_shows_in_the_remainder() {
    let pool = db().await;
    insert_request_log(&pool, &log("a", 1_000, 100, priced(3_000)))
        .await
        .unwrap();
    insert_request_log(&pool, &log("a", 500, 50, Cost::unknown()))
        .await
        .unwrap();

    let t = usage_totals(&pool, &window()).await.unwrap();
    assert_eq!(t.cost_micro, 3_000, "the unpriced call did not price as 0");
    assert_eq!(t.cost_unknown_requests, 1);
    assert_eq!(
        t.cost_unknown_tokens, 550,
        "the remainder states what the total does not cover"
    );
    // and the raw row really is NULL, not 0
    let c: Option<i64> = sqlx::query("SELECT cost_micro FROM request_logs ORDER BY id DESC")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("cost_micro");
    assert_eq!(c, None);
}

#[tokio::test]
async fn a_tool_row_is_not_an_unpriced_model_call() {
    let pool = db().await;
    let mut row = log("search", 0, 0, Cost::unknown());
    row.class = RequestClass::Tool;
    row.prompt_tokens = None;
    row.completion_tokens = None;
    insert_request_log(&pool, &row).await.unwrap();

    let t = usage_totals(&pool, &window()).await.unwrap();
    assert_eq!(t.requests, 1);
    assert_eq!(
        t.cost_unknown_requests, 0,
        "a tool execution has no money dimension to be missing"
    );
}

#[tokio::test]
async fn local_routes_are_free_not_unpriced() {
    // Regression: the four local classes route through *synthetic*
    // upstreams (-1/-2/-3/-4) that are never stored in `upstreams`, so a
    // `upstreams.get(id)` kind check missed every local request and priced
    // all of it `unknown`. That turns "unknown is not zero" — the rule the
    // whole feature rests on — into a permanent false alarm over exactly
    // the traffic that really is free.
    use crate::config::{
        AUDIO_UPSTREAM_ID, AUX_UPSTREAM_ID, IMAGE_UPSTREAM_ID, ROUTER_UPSTREAM_ID,
    };
    let snap = Snapshot::default();
    for id in [
        ROUTER_UPSTREAM_ID,
        AUX_UPSTREAM_ID,
        AUDIO_UPSTREAM_ID,
        IMAGE_UPSTREAM_ID,
    ] {
        let p = snap
            .prices_for("qwen3.8", Some(id), Some("qwen3.8"))
            .unwrap_or_else(|| panic!("upstream {id} priced as unknown"));
        assert_eq!(p.source, PriceSource::FreeLocal);
    }

    // …and a genuinely unknown cloud route stays unpriced.
    assert!(snap.prices_for("mystery", Some(7), Some("m")).is_none());
}

#[tokio::test]
async fn a_rebuild_reproduces_exactly_what_the_write_path_produced() {
    // The rollup and the rows it came from are two representations of one
    // truth; this is the test that they agree. It is also the upgrade path:
    // an install with a month of logs and no rollups must not open the
    // Usage page on an empty chart.
    let pool = db().await;
    for i in 0..30i64 {
        let cost = if i % 5 == 0 {
            Cost::unknown()
        } else {
            priced(i * 77)
        };
        let mut r = log("a", i * 3, i, cost);
        if i % 4 == 0 {
            r.requested_alias = "b".into();
        }
        if i % 9 == 0 {
            r.status = 503;
            r.error_kind = Some("gpu_hold".into());
        }
        if i % 7 == 0 {
            r.status = 500;
            r.error_kind = Some("upstream".into());
        }
        insert_request_log(&pool, &r).await.unwrap();
    }
    // A 200 carrying a non-refusal error_kind: the mid-stream failure, where
    // the headers are already out. It belongs in the latency distribution,
    // and an `error_kind IS NULL` rebuild filter would silently drop it.
    let mut mid_stream = log("a", 50, 5, priced(10));
    mid_stream.error_kind = Some("upstream".into());
    insert_request_log(&pool, &mid_stream).await.unwrap();

    let before = usage_totals(&pool, &window()).await.unwrap();
    let p95_before = usage_percentiles(&pool, &window(), "total", &[0.95])
        .await
        .unwrap();
    let rows_before = usage_series(&pool, &window(), Bucket::Hour, GroupBy::Alias)
        .await
        .unwrap();

    rebuild_usage(&pool).await.unwrap();

    let after = usage_totals(&pool, &window()).await.unwrap();
    assert_eq!(before, after, "a rebuild is not allowed to change a number");
    assert_eq!(
        rows_before,
        usage_series(&pool, &window(), Bucket::Hour, GroupBy::Alias)
            .await
            .unwrap(),
        "…nor any per-series cell, including total_min/total_max"
    );
    assert_eq!(
        p95_before,
        usage_percentiles(&pool, &window(), "total", &[0.95])
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_rebuild_keeps_the_decode_count_a_short_turn_produced() {
    // Reconstructing it as rate x duration truncated: a 1-token, 24 ms turn
    // at 41 tok/s came back as *zero* tokens, so a backfill lowered
    // throughput most exactly where turns were shortest.
    let pool = db().await;
    let mut r = log("qwen", 100, 1, Cost::unknown());
    r.timings = Some(Timings {
        prompt_n: 100,
        prompt_ms: 40.0,
        prompt_per_second: 2500.0,
        predicted_n: 1,
        predicted_ms: 24.25,
        predicted_per_second: 41.23,
        cache_n: Some(60),
        draft_n: None,
        draft_n_accepted: None,
    });
    insert_request_log(&pool, &r).await.unwrap();

    let before = usage_totals(&pool, &window()).await.unwrap();
    assert_eq!(before.decode_tokens, 1);
    rebuild_usage(&pool).await.unwrap();
    assert_eq!(
        usage_totals(&pool, &window()).await.unwrap().decode_tokens,
        1,
        "the rebuild reads the stored count, not rate x duration"
    );
}

#[tokio::test]
async fn backfill_runs_once_and_never_over_a_live_install() {
    let pool = db().await;
    insert_request_log(&pool, &log("a", 10, 1, priced(500)))
        .await
        .unwrap();

    // Rollups already exist (the write path made them), so backfill is a
    // no-op — rebuilding here would throw away every hour whose raw rows
    // the pruner has already taken.
    assert_eq!(backfill_usage_if_empty(&pool).await.unwrap(), 0);

    // The upgrade case: raw rows, no rollups.
    sqlx::query("DELETE FROM usage_hourly")
        .execute(&pool)
        .await
        .unwrap();
    assert!(backfill_usage_if_empty(&pool).await.unwrap() > 0);
    assert_eq!(
        usage_totals(&pool, &window()).await.unwrap().cost_micro,
        500
    );

    // A fresh install: nothing either side, nothing to do.
    let fresh = db().await;
    assert_eq!(backfill_usage_if_empty(&fresh).await.unwrap(), 0);
}

#[tokio::test]
async fn rollups_survive_the_retention_pruner() {
    let pool = db().await;
    for _ in 0..5 {
        insert_request_log(&pool, &log("a", 100, 10, priced(1_000)))
            .await
            .unwrap();
    }
    // Age every raw row past the window, then prune.
    sqlx::query("UPDATE request_logs SET ts = datetime('now', '-90 days')")
        .execute(&pool)
        .await
        .unwrap();
    let removed = prune_logs(&pool, 30, 0).await.unwrap();
    assert_eq!(removed, 5);

    let t = usage_totals(&pool, &window()).await.unwrap();
    assert_eq!(t.requests, 5, "history outlives the tail");
    assert_eq!(t.cost_micro, 5_000);
}

#[tokio::test]
async fn percentiles_come_from_the_distribution_not_a_mean() {
    let pool = db().await;
    // 99 fast requests and one very slow one: the mean is dragged up, p50
    // is not, p99 finds the outlier.
    for _ in 0..99 {
        let mut r = log("a", 10, 1, priced(1));
        r.total_ms = Some(100);
        insert_request_log(&pool, &r).await.unwrap();
    }
    let mut slow = log("a", 10, 1, priced(1));
    slow.total_ms = Some(30_000);
    insert_request_log(&pool, &slow).await.unwrap();

    let got = usage_percentiles(&pool, &window(), "total", &[0.5, 1.0])
        .await
        .unwrap();
    let p50 = got[0].unwrap();
    let p100 = got[1].unwrap();
    assert!((90.0..110.0).contains(&p50), "p50 was {p50}");
    assert!((27_000.0..33_000.0).contains(&p100), "p100 was {p100}");

    // The mean, for contrast: one outlier drags it 4x above the typical
    // request. This is why the rollup keeps a distribution and not an
    // average — and why a p95 must never be averaged across hours either.
    let t = usage_totals(&pool, &window()).await.unwrap();
    let mean = t.total_sum as f64 / t.total_count as f64;
    assert!(mean > 380.0, "mean was {mean}");
}

#[tokio::test]
async fn refusals_stay_out_of_the_latency_distribution() {
    let pool = db().await;
    let mut ok = log("a", 10, 1, priced(1));
    ok.total_ms = Some(5_000);
    insert_request_log(&pool, &ok).await.unwrap();
    // A hold refusal answers in a millisecond; letting it in would make the
    // p50 improve precisely when the gateway stops working.
    for _ in 0..50 {
        let mut refused = log("a", 0, 0, Cost::unknown());
        refused.status = 503;
        refused.error_kind = Some("gpu_hold".into());
        refused.total_ms = Some(1);
        insert_request_log(&pool, &refused).await.unwrap();
    }
    let p50 = usage_percentiles(&pool, &window(), "total", &[0.5])
        .await
        .unwrap()[0]
        .unwrap();
    assert!(p50 > 3_000.0, "p50 was {p50} — refusals leaked in");

    let t = usage_totals(&pool, &window()).await.unwrap();
    assert_eq!(t.refusals, 50, "but they are counted as refusals");
    assert_eq!(t.errors, 0, "and never as upstream errors");
}

#[tokio::test]
async fn an_empty_window_has_no_percentile() {
    let pool = db().await;
    assert_eq!(
        usage_percentiles(&pool, &window(), "total", &[0.95])
            .await
            .unwrap(),
        vec![None],
        "0 would draw a gateway that got infinitely fast"
    );
}

#[tokio::test]
async fn latency_buckets_round_trip_within_their_resolution() {
    for ms in [1i64, 7, 63, 512, 4_096, 65_536, 600_000] {
        let back = latency_bucket_ms(latency_bucket_idx(ms));
        let err = (back - ms as f64).abs() / ms as f64;
        assert!(err < 0.12, "{ms} ms read back as {back} ({err:.3} off)");
    }
}

#[tokio::test]
async fn manual_prices_win_over_catalog_and_local_is_always_free() {
    let pool = db().await;
    upsert_price(
        &pool,
        PriceScope::Alias,
        "gpt-x",
        "per_mtok",
        &Prices {
            price_in: Some(1.0),
            price_out: Some(2.0),
            source: PriceSource::Catalog,
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    upsert_price(
        &pool,
        PriceScope::Alias,
        "gpt-x",
        "per_mtok",
        &Prices {
            price_in: Some(9.0),
            price_out: Some(9.0),
            source: PriceSource::Manual,
            ..Default::default()
        },
        Some("negotiated"),
    )
    .await
    .unwrap();

    let mut snap = Snapshot {
        prices: list_prices(&pool).await.unwrap(),
        ..Default::default()
    };
    assert_eq!(snap.prices.len(), 2, "both sources coexist");
    let p = snap.prices_for("gpt-x", None, None).unwrap();
    assert_eq!(p.price_in, Some(9.0), "the owner's number wins");
    assert_eq!(p.source, PriceSource::Manual);

    // A second catalog refresh updates only the catalog row.
    upsert_price(
        &pool,
        PriceScope::Alias,
        "gpt-x",
        "per_mtok",
        &Prices {
            price_in: Some(1.5),
            price_out: Some(2.5),
            source: PriceSource::Catalog,
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    snap.prices = list_prices(&pool).await.unwrap();
    assert_eq!(snap.prices.len(), 2);
    assert_eq!(
        snap.prices_for("gpt-x", None, None).unwrap().price_in,
        Some(9.0)
    );

    // An unpriced alias is unpriced, not free.
    assert!(snap.prices_for("unknown-alias", None, None).is_none());
    let c = crate::pricing::price_request(
        &TokenUsage {
            prompt: Some(1000),
            completion: Some(10),
            ..Default::default()
        },
        snap.prices_for("unknown-alias", None, None).as_ref(),
    );
    assert_eq!(c.total_micro, None);
}
