//! Migration 0071: personality profiles (personality-profiles design §1.2)
//! — the `chat_profiles` table seeded with the built-in "Concise" once, and
//! `chat_threads.profile_id`, none for every existing thread.

use sqlx::Row;

use super::db_at_version;

#[tokio::test]
async fn migration_0071_applies_on_a_populated_db_and_seeds_concise_once() {
    let pool = db_at_version(70).await;
    sqlx::query("INSERT INTO chat_threads (id, model_alias, title) VALUES (7, 'm', 'old')")
        .execute(&pool)
        .await
        .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let rows = sqlx::query("SELECT * FROM chat_profiles")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String, _>("name"), "Concise");
    assert_eq!(
        rows[0].get::<Option<String>, _>("builtin").as_deref(),
        Some("concise")
    );
    assert_eq!(rows[0].get::<String, _>("body"), "{}");
    assert!(!rows[0].get::<String, _>("created_at").is_empty());
    let id: i64 = rows[0].get("id");

    // The thread that was there keeps no profile.
    let profile: Option<i64> =
        sqlx::query_scalar("SELECT profile_id FROM chat_threads WHERE id = 7")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(profile, None);

    // A start after it seeds nothing more, and a deleted built-in stays
    // deleted.
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    sqlx::query("DELETE FROM chat_profiles WHERE id = ?1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_profiles")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn migration_0071_enforces_the_name_rules_and_sets_threads_back_on_delete() {
    let pool = db_at_version(71).await;
    // Unique in any (ASCII) case; the built-in key unique too.
    let dup = sqlx::query("INSERT INTO chat_profiles (name) VALUES ('CONCISE')")
        .execute(&pool)
        .await;
    assert!(dup.is_err(), "names are unique without regard to case");
    let dup = sqlx::query("INSERT INTO chat_profiles (name, builtin) VALUES ('Other', 'concise')")
        .execute(&pool)
        .await;
    assert!(dup.is_err(), "one row per built-in");

    let id: i64 =
        sqlx::query_scalar("INSERT INTO chat_profiles (name) VALUES ('Calm') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO chat_threads (id, model_alias, profile_id) VALUES (9, 'm', ?1)")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let unknown =
        sqlx::query("INSERT INTO chat_threads (model_alias, profile_id) VALUES ('m', 999)")
            .execute(&pool)
            .await;
    assert!(unknown.is_err(), "a thread names a profile that exists");
    sqlx::query("DELETE FROM chat_profiles WHERE id = ?1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let profile: Option<i64> =
        sqlx::query_scalar("SELECT profile_id FROM chat_threads WHERE id = 9")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(profile, None);
}
