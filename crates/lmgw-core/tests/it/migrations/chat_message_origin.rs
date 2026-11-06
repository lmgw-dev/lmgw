//! Migration 0099: `chat_messages.origin` (chat-plane design §9.10), NULL
//! for every row from before it, which reads as a message that says
//! nothing of where it was written; a send's origin round-trips through the
//! store, and a rewrite keeps it unless it names one.

use lmgw_core::store::{self, SendMessageOutcome, UserMessage};

use super::db_at_version;

#[tokio::test]
async fn migration_0099_adds_origin_null_for_old_rows() {
    let pool = db_at_version(98).await;
    let thread: i64 = sqlx::query_scalar(
        "INSERT INTO chat_threads (title, model_alias) VALUES ('t', 'm') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chat_messages (thread_id, role, content, reasoning)
         VALUES (?1, 'user', 'Hello', '')",
    )
    .bind(thread)
    .execute(&pool)
    .await
    .unwrap();

    store::run_migrations(&pool).await.unwrap();

    let row = store::list_chat_messages(&pool, thread)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!((row.content.as_str(), row.origin), ("Hello", None));

    // Written with a send, read back; a rewrite without one keeps it, one
    // with one replaces it.
    let SendMessageOutcome::Sent(id) = store::append_user_message_by(
        &pool,
        thread,
        UserMessage {
            content: "From the sidebar",
            origin: Some("Typed in a sidebar."),
            ..Default::default()
        },
        &store::Decider::gateway(),
    )
    .await
    .unwrap() else {
        panic!("not sent");
    };
    let origin_of = || async {
        store::get_chat_message(&pool, thread, id)
            .await
            .unwrap()
            .unwrap()
            .origin
    };
    assert_eq!(origin_of().await.as_deref(), Some("Typed in a sidebar."));
    assert!(
        store::rewrite_chat_user_message(&pool, thread, id, "Edited", &[], None, None)
            .await
            .unwrap()
    );
    assert_eq!(origin_of().await.as_deref(), Some("Typed in a sidebar."));
    assert!(store::rewrite_chat_user_message(
        &pool,
        thread,
        id,
        "Edited",
        &[],
        Some("Typed on the phone."),
        None
    )
    .await
    .unwrap());
    assert_eq!(origin_of().await.as_deref(), Some("Typed on the phone."));
}
