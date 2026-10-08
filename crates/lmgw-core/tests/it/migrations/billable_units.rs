//! Migration 0070: billable units (billable-units design §5) — `prices`
//! rebuilt with the v1 unit set and one `price` column, every existing row,
//! id and the id high-water mark kept; `request_logs` and `usage_hourly`
//! gain the quantity columns with every old value kept, NULL and 0 in the
//! new ones; and the guard that refuses, by name, the rows the rebuild could
//! not carry.

use lmgw_core::config::PriceUnit;
use lmgw_core::store;
use sqlx::{AssertSqlSafe, SqlitePool};

use super::{applied_versions, db_at_version};

/// The last version before 0070.
const BEFORE: i64 = 69;

/// Every column of `table`, in order.
async fn columns(pool: &SqlitePool, table: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
        .bind(table)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// `cols` of every row of `table`, each value through `quote()`: exact for
/// every type, and NULL stays distinguishable from `''` and from 0.
async fn dump(pool: &SqlitePool, table: &str, cols: &[String], order: &str) -> Vec<String> {
    let row = cols
        .iter()
        .map(|c| format!("quote({c})"))
        .collect::<Vec<_>>()
        .join(" || '|' || ");
    sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT {row} FROM {table} ORDER BY {order}"
    )))
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn count(pool: &SqlitePool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn sequence(pool: &SqlitePool) -> Option<i64> {
    sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'prices'")
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn exec(pool: &SqlitePool, sql: &'static str) -> Result<(), sqlx::Error> {
    sqlx::query(sql).execute(pool).await.map(|_| ())
}

/// Price rows of every shape a version-69 table holds — catalog and manual,
/// alias and upstream-model scopes, NULL rates beside set ones — and a
/// deleted top id, so the sequence is above `max(id)`.
async fn seed_prices(pool: &SqlitePool) {
    exec(
        pool,
        "INSERT INTO prices (scope_kind, scope_key, unit, price_in, price_out, price_cache_read,
             price_cache_write, source, note, updated_at)
         VALUES ('alias', 'gpt-x', 'per_mtok', 3.0, 15.0, 0.3, 3.75, 'catalog', NULL,
                 '2026-01-02 10:00:00'),
                ('alias', 'gpt-x', 'per_mtok', 2.5, 10.0, NULL, NULL, 'manual', 'negotiated',
                 '2026-01-03 11:00:00'),
                ('upstream_model', '7:claude-x', 'per_mtok', NULL, 5.0, NULL, NULL, 'catalog',
                 NULL, '2026-01-04 12:00:00'),
                ('upstream_model', '7:gemini', 'per_mtok', 0.1, 0.4, 0.025, NULL, 'manual',
                 'by hand', '2026-01-05 13:00:00'),
                ('alias', 'top', 'per_mtok', 1.0, 1.0, NULL, NULL, 'manual', NULL,
                 '2026-01-06 14:00:00')",
    )
    .await
    .unwrap();
    exec(pool, "DELETE FROM prices WHERE scope_key = 'top'")
        .await
        .unwrap();
}

/// Request rows and their rollup as a version-69 write path left them:
/// priced, unpriced, local, an error, a tool call.
async fn seed_usage(pool: &SqlitePool) {
    exec(
        pool,
        "INSERT INTO request_logs (ts, client_key, ingress_proto, requested_alias, upstream_id,
             upstream_name, upstream_model, egress_proto, status, ttfb_ms, total_ms,
             prompt_tokens, completion_tokens, streamed, error_kind, error_msg, key_id, class,
             cost_micro, cost_in_micro, cost_out_micro, price_in, price_out, price_cache_read,
             price_cache_write, price_source, cached_in_tokens, cache_write_tokens,
             reasoning_tokens, prefill_ms, decode_ms, decode_tok_s, prompt_n, cache_n,
             predicted_n, fallback_reason, rung, degraded)
         VALUES ('2026-09-01 10:00:00', 'laptop', 'openai', 'gpt-x', 7, 'cloud', 'gpt-x',
                 'openai', 200, 120, 900, 1000, 100, 1, NULL, NULL, 3, 'chat',
                 4000, 2500, 1500, 2.5, 15.0, 0.25, 3.125, 'manual', 600, NULL,
                 40, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
                ('2026-09-01 10:05:00', NULL, 'openai', 'cloud-whisper', 7, 'cloud',
                 'whisper-1', 'openai', 200, NULL, 1500, NULL, NULL, 0, NULL, NULL, NULL,
                 'audio', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'unknown', NULL, NULL,
                 NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
                ('2026-09-01 11:00:00', NULL, 'anthropic', 'qwen3.8', -1, 'llama-router',
                 'qwen3.8', 'llama_cpp', 200, 80, 2400, 500, 50, 1, NULL, NULL, NULL, 'chat',
                 0, 0, 0, 0.0, 0.0, 0.0, 0.0, 'free_local', NULL, NULL, NULL, 40.5, 1210.25,
                 41.3, 500, 120, 50, 'hold', 2, 'fallback lacks vision'),
                ('2026-09-01 11:30:00', NULL, 'openai', 'gpt-x', 7, 'cloud', 'gpt-x',
                 'openai', 502, 300, 300, NULL, NULL, 0, 'upstream', 'bad gateway', NULL,
                 'chat', NULL, NULL, NULL, 2.5, 15.0, NULL, NULL, 'unknown', NULL, NULL, NULL,
                 NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
                ('2026-09-01 11:40:00', NULL, 'mcp', 'search', NULL, 'web', NULL, 'mcp', 200,
                 NULL, 75, NULL, NULL, 0, NULL, NULL, NULL, 'tool', NULL, NULL, NULL, NULL,
                 NULL, NULL, NULL, 'unknown', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                 NULL, NULL, NULL, NULL)",
    )
    .await
    .unwrap();
    exec(
        pool,
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, tokens_in, tokens_out, tokens_cached, tokens_cache_write,
             tokens_reasoning, cost_micro, cost_unknown_requests, cost_unknown_tokens, ttfb_sum,
             ttfb_count, total_sum, total_count, total_min, total_max, decode_tokens,
             decode_ms, prefill_ms, prompt_n, cache_n, draft_n, draft_accepted)
         VALUES ('2026-09-01T10', 3, 'gpt-x', 7, 'chat', 'ok', 1, 1000, 100, 600, 0, 40, 4000,
                 0, 0, 120, 1, 900, 1, 900, 900, 0, 0, 0, 0, 0, 0, 0),
                ('2026-09-01T10', 0, 'cloud-whisper', 7, 'audio', 'ok', 1, 0, 0, 0, 0, 0, 0,
                 1, 0, 0, 0, 1500, 1, 1500, 1500, 0, 0, 0, 0, 0, 0, 0),
                ('2026-09-01T11', 0, 'qwen3.8', -1, 'chat', 'ok', 1, 500, 50, 0, 0, 0, 0, 0, 0,
                 80, 1, 2400, 1, 2400, 2400, 50, 1210.25, 40.5, 500, 120, 0, 0),
                ('2026-09-01T11', 0, 'gpt-x', 7, 'chat', 'upstream_error', 1, 0, 0, 0, 0, 0, 0,
                 0, 0, 300, 1, 300, 1, 300, 300, 0, 0, 0, 0, 0, 0, 0)",
    )
    .await
    .unwrap();
}

/// An install at version 69 upgrades with every price row, request row and
/// rollup cell kept, and comes out with the new shape: the unit set, the
/// CHECK between token rates and `price`, NULL quantities on old rows and 0
/// in old rollups.
#[tokio::test]
async fn migration_0070_keeps_every_value_and_adds_the_billable_units() {
    let pool = db_at_version(BEFORE).await;
    seed_prices(&pool).await;
    seed_usage(&pool).await;

    let price_cols = columns(&pool, "prices").await;
    let log_cols = columns(&pool, "request_logs").await;
    let hourly_cols = columns(&pool, "usage_hourly").await;
    let hourly_order = "bucket_utc, key_id, alias, upstream_id, class, outcome";
    let prices_before = dump(&pool, "prices", &price_cols, "id").await;
    let logs_before = dump(&pool, "request_logs", &log_cols, "id").await;
    let hourly_before = dump(&pool, "usage_hourly", &hourly_cols, hourly_order).await;
    let seq_before = sequence(&pool).await;
    assert_eq!(
        seq_before,
        Some(5),
        "the deleted top id is the high-water mark"
    );
    assert_eq!(
        store::units_since(&pool).await.unwrap(),
        None,
        "nothing is recorded before the migration"
    );

    store::run_migrations(&pool)
        .await
        .expect("an install with ordinary price rows upgrades");
    assert!(applied_versions(&pool).await.contains(&70));

    // prices: every row, id and updated_at as it was, the sequence too.
    assert_eq!(
        dump(&pool, "prices", &price_cols, "id").await,
        prices_before
    );
    assert_eq!(sequence(&pool).await, seq_before);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prices WHERE price IS NOT NULL").await,
        0
    );
    let rows = store::list_prices(&pool).await.unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows
        .iter()
        .all(|r| r.unit == PriceUnit::PerMtok && r.price.is_none()));

    // The CHECKs and the unique index of the rebuilt table.
    exec(
        &pool,
        "INSERT INTO prices (scope_kind, scope_key, price_in) VALUES ('alias', 'raw', 1.0)",
    )
    .await
    .expect("an insert that names no unit is a token row, as before");
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM prices WHERE scope_key = 'raw' AND unit = 'per_mtok'"
        )
        .await,
        1
    );
    exec(
        &pool,
        "INSERT INTO prices (scope_kind, scope_key, unit, price)
         VALUES ('alias', 'whisper', 'per_audio_minute', 0.006)",
    )
    .await
    .expect("a per-minute rate in `price`");
    for (refused, why) in [
        (
            "INSERT INTO prices (scope_kind, scope_key, unit, price_in, price)
             VALUES ('alias', 'x', 'per_mtok', 1.0, 1.0)",
            "a token row carries no price",
        ),
        (
            "INSERT INTO prices (scope_kind, scope_key, unit, price_in)
             VALUES ('alias', 'x', 'per_request', 0.005)",
            "a fee is not an input-token rate",
        ),
        (
            "INSERT INTO prices (scope_kind, scope_key, unit, price)
             VALUES ('alias', 'x', 'per_second', 0.0001)",
            "per_second is gone",
        ),
        (
            "INSERT INTO prices (scope_kind, scope_key, unit, price)
             VALUES ('alias', 'x', 'per_video_second', 0.1)",
            "a reserved unit is not admitted yet",
        ),
        (
            "INSERT INTO prices (scope_kind, scope_key, source, unit, price_in)
             VALUES ('alias', 'gpt-x', 'catalog', 'per_mtok', 9.0)",
            "one row per (scope, source, unit)",
        ),
    ] {
        assert!(exec(&pool, refused).await.is_err(), "{why}");
    }

    // request_logs and usage_hourly: every old value kept, nothing measured.
    assert_eq!(
        dump(&pool, "request_logs", &log_cols, "id").await,
        logs_before
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM request_logs
             WHERE audio_in_ms IS NOT NULL OR chars_in IS NOT NULL OR images_out IS NOT NULL
                OR cost_units_micro IS NOT NULL OR price_per_audio_minute IS NOT NULL
                OR price_per_mchar IS NOT NULL OR price_per_image IS NOT NULL
                OR price_per_request IS NOT NULL"
        )
        .await,
        0,
        "old rows were never measured: NULL, not 0"
    );
    assert_eq!(
        dump(&pool, "usage_hourly", &hourly_cols, hourly_order).await,
        hourly_before
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM usage_hourly
             WHERE audio_in_ms <> 0 OR chars_in <> 0 OR images_out <> 0
                OR cost_unknown_audio_in_ms <> 0 OR cost_unknown_chars_in <> 0
                OR cost_unknown_images_out <> 0"
        )
        .await,
        0
    );

    // The queries read the migrated rollup, and the write path writes the
    // new columns into it.
    let window = store::UsageFilter {
        from: "2000-01-01T00".into(),
        to: "2999-01-01T00".into(),
        ..Default::default()
    };
    let t = store::usage_totals(&pool, &window).await.unwrap();
    assert_eq!(
        (t.requests, t.cost_micro, t.cost_unknown_requests),
        (4, 4000, 1)
    );
    let id = store::insert_request_log(
        &pool,
        &store::NewRequestLog {
            ingress_proto: "openai".into(),
            requested_alias: "cloud-whisper".into(),
            status: 200,
            audio_in_ms: Some(27_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let row = store::get_log(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.audio_in_ms, Some(27_000));
    let t = store::usage_totals(&pool, &window).await.unwrap();
    assert_eq!(
        (t.audio_in_ms, t.cost_unknown_audio_in_ms),
        (27_000, 27_000)
    );

    assert!(
        store::units_since(&pool).await.unwrap().is_some(),
        "quantities are recorded from the migration on"
    );
}

/// `per_second` and `per_char` rows, and a `per_request`/`per_image` row that
/// holds token rates, cannot be carried into the rebuilt table: the upgrade
/// stops before the migrator, names exactly those rows and says how to clear
/// them, and changes nothing. With them gone it goes through, and a
/// non-token row with no token rate is carried as it is.
#[tokio::test]
async fn rows_0070_cannot_carry_stop_the_upgrade_by_name_and_change_nothing() {
    let pool = db_at_version(BEFORE).await;
    exec(
        &pool,
        "INSERT INTO prices (id, scope_kind, scope_key, unit, price_in, price_out, source)
         VALUES (1, 'alias', 'gpt-x', 'per_mtok', 3.0, 15.0, 'catalog'),
                (2, 'alias', 'whisper', 'per_second', 0.0001, NULL, 'manual'),
                (3, 'upstream_model', '7:tts-1', 'per_char', NULL, 0.000015, 'manual'),
                (4, 'alias', 'lyria', 'per_request', NULL, 0.04, 'manual'),
                (5, 'alias', 'dall-e', 'per_image', NULL, NULL, 'manual')",
    )
    .await
    .unwrap();
    let price_cols = columns(&pool, "prices").await;
    let before = dump(&pool, "prices", &price_cols, "id").await;

    let err = store::run_migrations(&pool)
        .await
        .expect_err("rows the rebuild cannot carry stop the upgrade")
        .to_string();
    for named in [
        "id 2 (alias 'whisper', manual, per_second)",
        "id 3 (upstream_model '7:tts-1', manual, per_char)",
        "id 4 (alias 'lyria', manual, per_request)",
        "DELETE FROM prices WHERE id IN (2, 3, 4)",
    ] {
        assert!(err.contains(named), "{named:?} missing from: {err}");
    }
    for carried in ["id 1 (", "id 5 ("] {
        assert!(!err.contains(carried), "{carried:?} named in: {err}");
    }
    assert!(!applied_versions(&pool).await.contains(&70));
    assert_eq!(
        dump(&pool, "prices", &price_cols, "id").await,
        before,
        "nothing changed"
    );
    assert!(
        !columns(&pool, "prices")
            .await
            .contains(&"price".to_string()),
        "the table was not rebuilt"
    );

    // The owner clears them, and the next start goes through.
    exec(&pool, "DELETE FROM prices WHERE id IN (2, 3, 4)")
        .await
        .unwrap();
    store::run_migrations(&pool)
        .await
        .expect("with the rows cleared the upgrade goes through");
    let rows = store::list_prices(&pool).await.unwrap();
    let shapes: Vec<_> = rows
        .iter()
        .map(|r| (r.id, r.unit, r.price_in, r.price))
        .collect();
    assert_eq!(
        shapes,
        vec![
            (5, PriceUnit::PerImage, None, None),
            (1, PriceUnit::PerMtok, Some(3.0), None),
        ]
    );
}
