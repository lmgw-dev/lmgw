//! Migration 0086 over a database at 0085 holding a tracked download: the
//! row comes through whole (its revision columns too), and a tracked file
//! is its file at its place — the same repo file at another `dest_path` is
//! a row of its own, the same place twice is refused.

use super::db_at_version;

#[tokio::test]
async fn migration_0086_makes_the_place_part_of_a_downloads_identity() {
    let pool = db_at_version(85).await;
    sqlx::query(
        "INSERT INTO hf_models (repo, file, dest_path, target, status, etag,
                                requested_revision, resolved_commit)
         VALUES ('o/r', 'w.safetensors', 'o/r/w.safetensors', 'image', 'done', 'e1',
                 'main', 'c0ffee')",
    )
    .execute(&pool)
    .await
    .unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let kept: (i64, String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT id, status, requested_revision, resolved_commit FROM hf_models")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        kept,
        (1, "done".into(), Some("main".into()), Some("c0ffee".into()))
    );
    let insert = |dest: &'static str| {
        sqlx::query(
            "INSERT INTO hf_models (repo, file, dest_path, target)
             VALUES ('o/r', 'w.safetensors', ?1, 'image')",
        )
        .bind(dest)
        .execute(&pool)
    };
    assert!(insert("upscalers/w.safetensors").await.is_ok());
    assert!(insert("o/r/w.safetensors").await.is_err());
}
