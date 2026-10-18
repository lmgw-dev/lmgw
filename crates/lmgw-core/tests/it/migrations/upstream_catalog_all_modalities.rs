//! Migration 0080: `upstreams.catalog_all_modalities` (decision-models design
//! §6.2). Every existing row starts on, except an `openai` row on Gemini's
//! host, the one OpenAI-compatible endpoint the live check found refusing
//! `output_modalities=all`; the snapshot carries the rows that are off.

use sqlx::{AssertSqlSafe, SqlitePool};

use super::db_at_version;

async fn exec(pool: &SqlitePool, sql: &str) {
    sqlx::query(AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn migration_0080_turns_the_option_off_only_for_an_openai_row_on_geminis_host() {
    let pool = db_at_version(79).await;
    exec(
        &pool,
        "INSERT INTO upstreams (id, name, protocol, kind, base_url) VALUES \
         (1, 'openrouter', 'openai', 'generic', 'https://openrouter.ai/api/v1'), \
         (2, 'gemini-compat', 'openai', 'generic', \
          'https://generativelanguage.googleapis.com/v1beta/openai'), \
         (3, 'aistudio', 'gemini', 'generic', 'https://generativelanguage.googleapis.com'), \
         (4, 'anthropic', 'anthropic', 'generic', 'https://api.anthropic.com')",
    )
    .await;

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let on: Vec<(i64, i64)> =
        sqlx::query_as("SELECT id, catalog_all_modalities FROM upstreams ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(on, [(1, 1), (2, 0), (3, 1), (4, 1)]);
    let snap = lmgw_core::store::load_snapshot(&pool).await.unwrap();
    assert_eq!(
        snap.catalog_bare.iter().copied().collect::<Vec<_>>(),
        [2],
        "the snapshot names the row that asks bare"
    );
    // The CHECK holds the column to a flag.
    let refused = sqlx::query("UPDATE upstreams SET catalog_all_modalities = 2 WHERE id = 1")
        .execute(&pool)
        .await;
    assert!(refused.is_err(), "{refused:?}");
}
