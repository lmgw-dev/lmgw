//! Migration 0083: the lab store's three tables (labs-store design §1.2) on
//! a database at 0082. A group of any of the three labs and of no other, a
//! run per cell, artifact rows of the group and of a run with a data URL's
//! head where one came as typed, the artifact indexes (a run's outputs are
//! read by `run_id`); deleting a group takes its runs and artifact rows with
//! it.

use super::db_at_version;

#[tokio::test]
async fn migration_0083_adds_the_lab_store() {
    let pool = db_at_version(82).await;
    lmgw_core::store::run_migrations(&pool).await.unwrap();

    for lab in ["decision", "image", "audio"] {
        sqlx::query("INSERT INTO lab_groups (lab, title, form, plan) VALUES (?1, 't', '{}', '[]')")
            .bind(lab)
            .execute(&pool)
            .await
            .unwrap();
    }
    let refused = sqlx::query(
        "INSERT INTO lab_groups (lab, title, form, plan) VALUES ('chat', 't', '{}', '[]')",
    )
    .execute(&pool)
    .await;
    assert!(refused.is_err(), "the lab CHECK: {refused:?}");

    let created: String = sqlx::query_scalar("SELECT created_at FROM lab_groups WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        created.len() == 24 && created.ends_with('Z') && created.as_bytes()[19] == b'.',
        "UTC with milliseconds, as an export's filter compares it: {created}"
    );

    let run = "INSERT INTO lab_runs (group_id, cell, variant, endpoint, model_asked, answered_by,
                                     http_status, request, latency_ms)
               VALUES (1, ?1, 'as_written', '/v1/systemone', 'm', 'm', 200, '{}', 3)";
    sqlx::query(run).bind(0).execute(&pool).await.unwrap();
    assert!(
        sqlx::query(run).bind(0).execute(&pool).await.is_err(),
        "one run per cell"
    );
    let headers: String = sqlx::query_scalar("SELECT headers FROM lab_runs WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(headers, "{}");

    sqlx::query(
        "INSERT INTO lab_artifacts (group_id, run_id, role, ord, name, mime, size, sha256,
                                    data_url_head)
         VALUES (1, NULL, 'input', 0, 'a.png', 'image/png', 3, 'aa',
                 'data:image/x-receipt;name=a.png;base64'),
                (1, 1, 'output', 0, 'b.png', 'image/png', 3, 'bb', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let head: Option<String> =
        sqlx::query_scalar("SELECT data_url_head FROM lab_artifacts WHERE sha256 = 'aa'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        head.as_deref(),
        Some("data:image/x-receipt;name=a.png;base64")
    );
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'lab_artifacts'
           AND name LIKE 'idx_%' ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        indexes,
        [
            "idx_lab_artifacts_group",
            "idx_lab_artifacts_run",
            "idx_lab_artifacts_sha"
        ]
    );
    let refused = sqlx::query(
        "INSERT INTO lab_artifacts (group_id, role, ord, name, mime, size, sha256)
         VALUES (1, 'other', 0, 'c', 'x', 1, 'cc')",
    )
    .execute(&pool)
    .await;
    assert!(refused.is_err(), "the role CHECK: {refused:?}");

    sqlx::query("DELETE FROM lab_groups WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let left: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM lab_runs), (SELECT COUNT(*) FROM lab_artifacts)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, (0, 0), "the cascade");
}
