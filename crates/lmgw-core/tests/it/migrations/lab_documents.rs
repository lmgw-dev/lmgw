//! Migration 0115: `lab_groups.lab` and `lab_presets.lab` gain `'documents'`
//! by a table rebuild (document-conversion design WP12), run here over a
//! version-114 database holding the history of all three existing labs: a
//! set run's group, runs, artifacts, and presets a batch names. Every row
//! and id stays, the cascades do not fire during the rebuild, no reference
//! is left dangling, the id high-water marks hold, and a replay after the
//! commit changes nothing.

use sqlx::{AssertSqlSafe, Row, SqlitePool};

use super::db_at_version;

/// The migration under test, as sqlx runs it.
const MIGRATION: &str = include_str!("../../../migrations/0115_lab_documents.sql");

async fn exec(pool: &SqlitePool, sql: &'static str) {
    sqlx::query(sql).execute(pool).await.unwrap();
}

async fn count(pool: &SqlitePool, table: &'static str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Every column of every group and preset, as text, by id: what must come
/// through verbatim.
async fn dump(pool: &SqlitePool) -> (Vec<String>, Vec<String>) {
    let rows = |sql: &'static str| async move {
        sqlx::query(sql)
            .fetch_all(pool)
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (0..r.len())
                    .map(|i| format!("{:?}", r.try_get::<Option<String>, _>(i).ok().flatten()))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
    };
    (
        rows(
            "SELECT CAST(id AS TEXT), lab, title, form, plan, override, created_at, \
             CAST(batch_id AS TEXT), CAST(item_ix AS TEXT), expected, item_key \
             FROM lab_groups ORDER BY id",
        )
        .await,
        rows(
            "SELECT CAST(id AS TEXT), lab, kind, name, body, created_at, updated_at \
             FROM lab_presets ORDER BY id",
        )
        .await,
    )
}

async fn seed(pool: &SqlitePool) {
    exec(
        pool,
        "INSERT INTO lab_presets (lab, kind, name, body) VALUES
            ('decision', 'question_set', 'triage', '{\"q\":1}'),
            ('image', 'form', 'night', '{}'),
            ('audio', 'form', 'calm', '{}')",
    )
    .await;
    exec(
        pool,
        "INSERT INTO lab_batches (lab, preset_id, preset_snapshot, model, source_name,
             mapping, item_count, concurrency)
         VALUES ('decision', 1, '{}', 'decide/laya', 'tickets.example.csv', '{}', 2, 1)",
    )
    .await;
    exec(
        pool,
        "INSERT INTO lab_groups (lab, title, form, plan, override) VALUES
            ('decision', 'a decision', '{\"a\":1}', '[]', '{\"o\":1}'),
            ('image', 'an image', '{}', '[]', NULL),
            ('audio', 'a clip', '{}', '[]', NULL)",
    )
    .await;
    // A set run's items: groups of the batch with their key and expectation.
    exec(
        pool,
        "INSERT INTO lab_groups (lab, title, form, plan, batch_id, item_ix, expected, item_key)
         VALUES ('decision', 'item 0', '{}', '[]', 1, 0, '\"x\"', 'k0'),
                ('decision', 'item 1', '{}', '[]', 1, 1, NULL, 'k1')",
    )
    .await;
    // A fifth group, deleted again: the sequence stands at 6.
    exec(
        pool,
        "INSERT INTO lab_groups (lab, title, form, plan) VALUES ('image', 'gone', '{}', '[]')",
    )
    .await;
    exec(pool, "DELETE FROM lab_groups WHERE title = 'gone'").await;
    exec(
        pool,
        "INSERT INTO lab_runs (group_id, cell, variant, endpoint, model_asked, answered_by,
             http_status, request, latency_ms)
         VALUES (1, 0, 'v', '/v1/x', 'm', 'm', 200, '{}', 5),
                (4, 0, 'v', '/v1/x', 'm', 'm', 200, '{}', 5),
                (5, 0, 'v', '/v1/x', 'm', 'm', 200, '{}', 5)",
    )
    .await;
    exec(
        pool,
        "INSERT INTO lab_artifacts (group_id, run_id, role, ord, name, mime, size, sha256)
         VALUES (2, NULL, 'input', 0, 'in.png', 'image/png', 3, 'aa'),
                (2, NULL, 'output', 0, 'out.png', 'image/png', 3, 'bb'),
                (4, 2, 'output', 0, 'o.txt', 'text/plain', 1, 'cc')",
    )
    .await;
}

#[tokio::test]
async fn migration_0115_keeps_every_lab_row_and_admits_documents() {
    let pool = db_at_version(114).await;
    seed(&pool).await;
    let before = dump(&pool).await;
    let counts_before = (
        count(&pool, "lab_runs").await,
        count(&pool, "lab_artifacts").await,
        count(&pool, "lab_batches").await,
    );
    let top: i64 = sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'lab_groups'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(top, 6, "the deleted group had the top id");

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    assert_eq!(dump(&pool).await, before, "every row verbatim, ids kept");
    assert_eq!(count(&pool, "lab_groups").await, 5);
    assert_eq!(count(&pool, "lab_presets").await, 3);
    assert_eq!(
        (
            count(&pool, "lab_runs").await,
            count(&pool, "lab_artifacts").await,
            count(&pool, "lab_batches").await,
        ),
        counts_before,
        "nothing cascaded away"
    );
    let dangling = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(dangling.is_empty(), "no dangling reference");
    let preset: Option<i64> = sqlx::query_scalar("SELECT preset_id FROM lab_batches WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(preset, Some(1), "the batch still names its preset");
    let batch: Option<i64> = sqlx::query_scalar("SELECT batch_id FROM lab_groups WHERE id = 4")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(batch, Some(1), "a set run's item still belongs to it");

    // The lab CHECKs now admit documents, and still refuse anything else.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO lab_groups (lab, title, form, plan) VALUES ('documents', 'd', '{}', '[]')
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(id, 7, "the deleted top id is not handed out again");
    exec(
        &pool,
        "INSERT INTO lab_presets (lab, kind, name, body)
         VALUES ('documents', 'document_options', 'p', '{}')",
    )
    .await;
    assert!(sqlx::query(
        "INSERT INTO lab_groups (lab, title, form, plan) VALUES ('other', 'd', '{}', '[]')"
    )
    .execute(&pool)
    .await
    .is_err());
    // The cascades still work after the rebuild: deleting the batch takes its
    // items, deleting a group takes its runs and files.
    exec(&pool, "DELETE FROM lab_batches WHERE id = 1").await;
    assert_eq!(count(&pool, "lab_groups").await, 4, "items went with it");
    exec(&pool, "DELETE FROM lab_groups WHERE id = 2").await;
    assert_eq!(count(&pool, "lab_artifacts").await, 0);
}

/// A crash between the file's COMMIT and sqlx's record replays the whole
/// file against the new tables: rows, ids and the sequence stay.
#[tokio::test]
async fn a_replay_of_0115_after_its_commit_changes_nothing() {
    let pool = db_at_version(114).await;
    seed(&pool).await;
    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
    let once = dump(&pool).await;
    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
    assert_eq!(dump(&pool).await, once);
    let seq: i64 = sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'lab_groups'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(seq, 6);
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    assert_eq!(dump(&pool).await, once);
    assert_eq!(count(&pool, "lab_runs").await, 3);
    assert!(sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap()
        .is_empty());
}
