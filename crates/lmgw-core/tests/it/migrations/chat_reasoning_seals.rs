//! Migration 0096: `chat_messages.reasoning_seals` and
//! `native_response_items.encrypted_sha256` (openai egress design
//! §7.3.2, its hashes moved to `native_response_item_contents` by 0100),
//! NULL for every row from before it, which reads as no seals.

use super::db_at_version;

#[tokio::test]
async fn migration_0096_adds_reasoning_seals_null_for_old_rows() {
    let pool = db_at_version(95).await;
    let thread: i64 = sqlx::query_scalar(
        "INSERT INTO chat_threads (title, model_alias) VALUES ('t', 'm') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chat_messages (thread_id, role, content, reasoning)
         VALUES (?1, 'assistant', 'Hello', 'Hm.')",
    )
    .bind(thread)
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let seals: Option<String> =
        sqlx::query_scalar("SELECT reasoning_seals FROM chat_messages WHERE thread_id = ?1")
            .bind(thread)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(seals, None);
    let row = lmgw_core::store::list_chat_messages(&pool, thread)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        (row.reasoning.as_str(), row.reasoning_seals.len()),
        ("Hm.", 0)
    );
    // And a recorded item's encrypted content's hash (WP8 review S-R3, one
    // per variant since 0100), none for the items from before it.
    let hashed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM native_response_item_contents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hashed, 0);
}
