//! Migration 0084: presets and set runs (labs-store design §3.1) on a
//! database at 0083 holding a group of the history. The history's group
//! keeps no batch; a preset name is unique per lab and kind; a set run is
//! written not ready; a set run's items are groups with a key that go with
//! it, and its preset's delete leaves it its snapshot; a run's `readable` is
//! a flag or nothing.

use super::db_at_version;

#[tokio::test]
async fn migration_0084_adds_presets_and_set_runs() {
    let pool = db_at_version(83).await;
    sqlx::query(
        "INSERT INTO lab_groups (lab, title, form, plan) VALUES ('decision', 't', '{}', '[]')",
    )
    .execute(&pool)
    .await
    .unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let old: (Option<i64>, Option<i64>, Option<String>) =
        sqlx::query_as("SELECT batch_id, item_ix, expected FROM lab_groups WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(old, (None, None, None), "a group of the history");

    let preset = "INSERT INTO lab_presets (lab, kind, name, body)
                  VALUES ('decision', 'question_set', ?1, '{}')";
    sqlx::query(preset)
        .bind("triage")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query(preset)
            .bind("triage")
            .execute(&pool)
            .await
            .is_err(),
        "one name per lab and kind"
    );
    let stamps: (String, String) =
        sqlx::query_as("SELECT created_at, updated_at FROM lab_presets WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        stamps.0.ends_with('Z') && stamps.1.ends_with('Z'),
        "{stamps:?}"
    );

    let batch = "INSERT INTO lab_batches (lab, preset_id, preset_snapshot, model, source_name,
                     mapping, item_count, concurrency)
                 VALUES ('decision', 1, '{}', 'decide/laya', 'tickets.csv', '{}', 2, ?1)";
    assert!(
        sqlx::query(batch).bind(0).execute(&pool).await.is_err(),
        "a concurrency of at least 1"
    );
    sqlx::query(batch).bind(1).execute(&pool).await.unwrap();
    let ready: i64 = sqlx::query_scalar("SELECT ready FROM lab_batches WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ready, 0, "a set run is written not ready");
    for ix in 0..2 {
        sqlx::query(
            "INSERT INTO lab_groups (lab, title, form, plan, batch_id, item_ix, expected, item_key)
             VALUES ('decision', 'i', '{}', '[]', 1, ?1, '{\"team\":\"billing\"}', ?2)",
        )
        .bind(ix)
        .bind(format!("T-{ix}"))
        .execute(&pool)
        .await
        .unwrap();
    }
    let run = "INSERT INTO lab_runs (group_id, cell, variant, endpoint, model_asked,
                   answered_by, http_status, request, latency_ms, readable)
               VALUES (2, 0, 'as_written', '/v1/systemone', 'm', 'm', 200, '{}', 1, ?1)";
    assert!(
        sqlx::query(run).bind(2).execute(&pool).await.is_err(),
        "readable is 0 or 1"
    );
    sqlx::query(run).bind(0).execute(&pool).await.unwrap();

    sqlx::query("DELETE FROM lab_presets WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let kept: (Option<i64>, String) =
        sqlx::query_as("SELECT preset_id, preset_snapshot FROM lab_batches WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kept, (None, "{}".to_string()), "the snapshot stays");

    sqlx::query("DELETE FROM lab_batches WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM lab_groups ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, [1], "the items go with their run, the history stays");
}
