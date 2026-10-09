//! Migration 0075: a late MCP task result's row (MCP Tasks design §2.2) —
//! `chat_messages.task`, none on every message that was there, and a row of
//! role `tool` reading back with its facts.

use super::db_at_version;

#[tokio::test]
async fn migration_0075_applies_on_a_populated_db_and_a_result_row_reads_back() {
    let pool = db_at_version(74).await;
    sqlx::query("INSERT INTO chat_threads (id, model_alias, title) VALUES (7, 'm', 'old')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO chat_messages (thread_id, role, content) VALUES (7, 'user', 'hi')")
        .execute(&pool)
        .await
        .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let task: Option<String> = sqlx::query_scalar("SELECT task FROM chat_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(task, None);
    sqlx::query(
        "INSERT INTO chat_messages (thread_id, role, content, task)
         VALUES (7, 'tool', 'job t1 (desktop__build) completed', ?1)",
    )
    .bind(
        r#"{"task_id":"t1","server_label":"desktop","tool":"desktop__build","status":"completed","ended_by":null}"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    let rows = lmgw_core::store::list_chat_messages(&pool, 7)
        .await
        .unwrap();
    assert!(!rows[0].is_task_result());
    assert!(rows[1].is_task_result());
    assert_eq!(rows[1].task.as_ref().unwrap().status, "completed");
}
