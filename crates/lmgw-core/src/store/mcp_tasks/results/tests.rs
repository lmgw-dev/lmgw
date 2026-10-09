//! The bound session's read (module doc of `super`): the newest reply, the
//! result rows past the smaller bound, the watermark that outlives a
//! deleted row, and the plan that walks no thread.

use super::*;
use crate::store::{create_chat_thread, open_in_memory};

async fn add(pool: &SqlitePool, thread: i64, role: &str) -> i64 {
    let task = (role == "tool").then_some(
        r#"{"task_id":"t1","server_label":"desktop","tool":"desktop__build","status":"completed"}"#,
    );
    sqlx::query(
        "INSERT INTO chat_messages (thread_id, role, content, ir_messages, task) \
         VALUES (?1, ?2, 'x', ?3, ?4)",
    )
    .bind(thread)
    .bind(role)
    .bind(task.map(|_| "[]"))
    .bind(task)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid()
}

fn ids(read: &ResultsRead) -> Vec<i64> {
    read.rows.iter().map(|m| m.id).collect()
}

#[tokio::test]
async fn it_reads_the_unanswered_and_the_new_results_and_a_watermark_past_deletes() {
    let pool = open_in_memory().await.unwrap();
    let t = create_chat_thread(&pool, "m", "chat").await.unwrap();
    let other = create_chat_thread(&pool, "m", "chat").await.unwrap();
    // No rows at all.
    let empty = results_read(&pool, t, None).await.unwrap();
    assert_eq!(
        (empty.last_reply, empty.any, ids(&empty)),
        (0, false, vec![])
    );

    let _u = add(&pool, t, "user").await;
    let a1 = add(&pool, t, "assistant").await;
    let r1 = add(&pool, t, "tool").await;
    let a2 = add(&pool, t, "assistant").await;
    let r2 = add(&pool, t, "tool").await;
    // Another thread's rows count for nothing but the watermark.
    add(&pool, other, "tool").await;
    let last = add(&pool, other, "assistant").await;

    // No watermark yet: only what no reply answered.
    let read = results_read(&pool, t, None).await.unwrap();
    assert_eq!(read.last_reply, a2);
    assert!(read.any);
    assert_eq!(ids(&read), [r2]);
    assert_eq!(read.watermark, last, "the largest id handed out");
    let row = &read.rows[0];
    assert_eq!((row.role.as_str(), row.thread_id), ("tool", t));
    assert_eq!(row.ir_messages.as_deref(), Some("[]"));
    assert_eq!(row.task.as_ref().unwrap().task_id, "t1");

    // A watermark before the reply: the results since it too.
    assert_eq!(
        ids(&results_read(&pool, t, Some(a1)).await.unwrap()),
        [r1, r2]
    );
    assert_eq!(
        ids(&results_read(&pool, t, Some(last)).await.unwrap()),
        [r2]
    );

    // The newest row deleted: the watermark still covers its id.
    sqlx::query("DELETE FROM chat_messages WHERE id = ?1")
        .bind(last)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(results_read(&pool, t, None).await.unwrap().watermark, last);
    // The reply deleted: the result before it is unanswered again.
    sqlx::query("DELETE FROM chat_messages WHERE id = ?1")
        .bind(a2)
        .execute(&pool)
        .await
        .unwrap();
    let read = results_read(&pool, t, Some(last)).await.unwrap();
    assert_eq!((read.last_reply, ids(&read)), (a1, vec![r1, r2]));
}

#[tokio::test]
async fn it_walks_no_thread() {
    let pool = open_in_memory().await.unwrap();
    let plan: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {READ}")))
        .bind(1_i64)
        .bind(0_i64)
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("detail"))
        .collect();
    // The newest reply, the any-result probe and the rows: each a search
    // of the index, and no step scans the table.
    let searches = plan
        .iter()
        .filter(|d| d.starts_with("SEARCH") && d.contains("idx_chat_messages_thread_role"))
        .count();
    assert_eq!(searches, 3, "{plan:#?}");
    assert!(
        !plan
            .iter()
            .any(|d| d.starts_with("SCAN") && (d.contains("chat_messages") || d.contains(" m"))),
        "{plan:#?}"
    );
}
