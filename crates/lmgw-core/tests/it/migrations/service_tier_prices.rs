//! Migration 0092: service-tier pricing (openai egress design §5.3) —
//! `prices.service_tier` with its CHECK, every existing row `standard` with
//! its id, values and the id high-water mark kept; the unique index rebuilt
//! with the tier; `request_logs.service_tier` and `.price_note`, NULL on
//! every old row.

use lmgw_core::config::{PriceScope, PriceTier, PriceUnit};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::store;
use sqlx::SqlitePool;

use super::db_at_version;

/// The last version before 0092.
const BEFORE: i64 = 91;

async fn exec(pool: &SqlitePool, sql: &'static str) -> Result<(), sqlx::Error> {
    sqlx::query(sql).execute(pool).await.map(|_| ())
}

/// Price rows of a version-91 table, a common shape among them (manual
/// OpenAI rows on an upstream-model scope), and a deleted top id.
async fn seed(pool: &SqlitePool) {
    exec(
        pool,
        "INSERT INTO prices (scope_kind, scope_key, unit, price_in, price_out, price_cache_read,
             price_cache_write, price, source, note, updated_at)
         VALUES ('upstream_model', '6:gpt-5.6-sol', 'per_mtok', 4.0, 20.0, 0.4, 5.0, NULL,
                 'manual', 'pricing page', '2026-10-07 10:00:00'),
                ('upstream_model', '6:whisper-1', 'per_audio_minute', NULL, NULL, NULL, NULL,
                 0.006, 'manual', NULL, '2026-10-07 10:01:00'),
                ('alias', 'kilo-x', 'per_mtok', 1.0, 2.0, NULL, NULL, NULL, 'catalog', NULL,
                 '2026-10-08 09:00:00'),
                ('alias', 'top', 'per_mtok', 1.0, 1.0, NULL, NULL, NULL, 'manual', NULL,
                 '2026-10-08 09:30:00')",
    )
    .await
    .unwrap();
    exec(pool, "DELETE FROM prices WHERE scope_key = 'top'")
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO request_logs (ts, ingress_proto, requested_alias, status, streamed,
             price_source)
         VALUES ('2026-10-20 10:00:00', 'openai', 'gpt', 200, 0, 'manual')",
    )
    .await
    .unwrap();
}

/// Every price row's values, each through `quote()`, by id: what any
/// version's table holds.
async fn raw(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT quote(id) || '|' || quote(scope_kind) || '|' || quote(scope_key) || '|' ||
                quote(unit) || '|' || quote(price_in) || '|' || quote(price_out) || '|' ||
                quote(price_cache_read) || '|' || quote(price_cache_write) || '|' ||
                quote(price) || '|' || quote(source) || '|' || quote(note) || '|' ||
                quote(updated_at)
           FROM prices ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn migration_0092_makes_every_price_row_standard_and_keeps_it_whole() {
    let pool = db_at_version(BEFORE).await;
    seed(&pool).await;
    // At 91 the table has no tier column, which today's reader needs:
    // compare the raw values.
    let raw_before = raw(&pool).await;
    store::run_migrations(&pool).await.unwrap();
    assert_eq!(raw(&pool).await, raw_before, "every row, id and value kept");
    let rows = store::list_prices(&pool).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().all(|r| r.service_tier == PriceTier::Standard),
        "{rows:?}"
    );

    // The old request row: no tier, no note.
    let (tier, note): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT service_tier, price_note FROM request_logs")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((tier, note), (None, None));

    // The CHECK refuses a tier OpenAI does not price by; the index takes a
    // flex row beside the standard one, and a new id stays above the
    // deleted top one.
    assert!(exec(
        &pool,
        "INSERT INTO prices (scope_kind, scope_key, unit, price_in, source, service_tier)
         VALUES ('alias', 'x', 'per_mtok', 1.0, 'manual', 'priority')",
    )
    .await
    .is_err());
    let flex = Prices {
        price_in: Some(2.0),
        price_out: Some(10.0),
        source: PriceSource::Manual,
        ..Default::default()
    };
    store::upsert_price(
        &pool,
        PriceScope::UpstreamModel,
        "6:gpt-5.6-sol",
        PriceUnit::PerMtok,
        PriceTier::Flex,
        &flex,
        None,
        Some("Batch rates"),
    )
    .await
    .unwrap();
    // A second write of the same key updates its row, not a third one.
    store::upsert_price(
        &pool,
        PriceScope::UpstreamModel,
        "6:gpt-5.6-sol",
        PriceUnit::PerMtok,
        PriceTier::Flex,
        &flex,
        None,
        Some("Batch rates, checked"),
    )
    .await
    .unwrap();
    let rows = store::list_prices(&pool).await.unwrap();
    let sol: Vec<_> = rows
        .iter()
        .filter(|r| r.scope_key == "6:gpt-5.6-sol")
        .collect();
    assert_eq!(sol.len(), 2, "{sol:?}");
    assert_eq!(sol[0].service_tier, PriceTier::Standard, "standard first");
    assert_eq!(sol[0].price_in, Some(4.0), "the standard row is untouched");
    assert_eq!(sol[1].service_tier, PriceTier::Flex);
    assert_eq!(sol[1].note.as_deref(), Some("Batch rates, checked"));
    assert!(sol[1].id > 4, "above the deleted top id: {sol:?}");
}
