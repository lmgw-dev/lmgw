//! Migration 0085 (chat-media-tools design §9) over a database at 0084
//! holding a Chat attachment and an image row: every existing attachment is
//! an upload with no call, the image row has no prompt style or preview mode
//! of its own (the recipe's, and the default), and the columns' CHECKs hold.

use super::db_at_version;

#[tokio::test]
async fn migration_0085_makes_every_existing_attachment_an_upload() {
    let pool = db_at_version(84).await;
    let thread: i64 = sqlx::query_scalar(
        "INSERT INTO chat_threads (title, model_alias) VALUES ('t', 'm') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chat_attachments (thread_id, kind, name, mime, size, data)
         VALUES (?1, 'image', 'cat.png', 'image/png', 3, ?2)",
    )
    .bind(thread)
    .bind(vec![1u8, 2, 3])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO image_models (model_id, files, args, modes, edit, enabled)
         VALUES ('z', ?1, '{}', '[]', 0, 1)",
    )
    .bind(r#"{"diffusion_model": "z.gguf"}"#)
    .execute(&pool)
    .await
    .unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let (source, call): (String, Option<String>) =
        sqlx::query_as("SELECT source, call_id FROM chat_attachments")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((source.as_str(), call), ("upload", None));
    let (style, preview): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT prompt_style, preview_mode FROM image_models")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((style, preview), (None, None));
    for bad in [
        "UPDATE chat_attachments SET source = 'pasted'",
        "UPDATE image_models SET prompt_style = 'prose'",
        "UPDATE image_models SET preview_mode = 'fast'",
    ] {
        assert!(sqlx::query(bad).execute(&pool).await.is_err(), "{bad}");
    }
}
