//! A tool's file in the store (design §6.3): stored unbound when its call
//! ends, never a draft, bound to the reply in the reply's own write, gone
//! when no reply takes it — and never replayed as an upload.

use lmgw_core::store::{self, ChatReply, DeleteAttachmentOutcome, NewAttachment};

async fn tool_file(db: &sqlx::SqlitePool, thread: i64, call: &str) -> i64 {
    let mut a = NewAttachment::plain("image", "image-z-1.png", "image/png", &super::png());
    a.source = "tool".into();
    a.call_id = Some(call.into());
    store::insert_chat_attachment_new(db, thread, &a)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_tool_file_is_no_draft_and_binds_to_its_reply() {
    let db = store::open_in_memory().await.unwrap();
    let t = store::create_chat_thread(&db, "m", "chat").await.unwrap();
    let upload = store::insert_chat_attachment_new(
        &db,
        t,
        &NewAttachment::plain("image", "cat.png", "image/png", &super::png()),
    )
    .await
    .unwrap();
    let made = tool_file(&db, t, "c1").await;

    // Waiting for its reply: not listed, not a draft, not deletable as one.
    let listed: Vec<i64> = store::list_chat_attachments_meta(&db, t)
        .await
        .unwrap()
        .iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(listed, vec![upload]);
    assert!(store::list_draft_chat_attachments_by_ids(&db, t, &[made])
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        store::delete_draft_chat_attachment(&db, made)
            .await
            .unwrap(),
        DeleteAttachmentOutcome::NotFound
    ));
    // A send cannot take it either.
    let sent = store::append_user_message_with_attachments(&db, t, "hi", &[made])
        .await
        .unwrap();
    assert!(matches!(
        sent,
        store::SendMessageOutcome::AttachmentNotDraft
    ));

    // The reply binds it in its own write.
    let reply = store::append_chat_reply(
        &db,
        t,
        &ChatReply {
            content: "here".into(),
            tool_files: vec![made],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let meta = store::list_chat_attachments_meta(&db, t).await.unwrap();
    let bound = meta
        .iter()
        .find(|a| a.id == made)
        .expect("listed once bound");
    assert_eq!(bound.message_id, Some(reply));
    assert_eq!(bound.source, "tool");
    assert_eq!(bound.call_id.as_deref(), Some("c1"));

    // It is replayed through the record that names it, never as an upload.
    let user = store::append_user_message_with_attachments(&db, t, "look", &[upload])
        .await
        .unwrap();
    assert!(matches!(user, store::SendMessageOutcome::Sent(_)));
    let replayed: Vec<i64> = store::list_sent_chat_attachments(&db, t, None)
        .await
        .unwrap()
        .iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(replayed, vec![upload]);
    let by_id = store::chat_attachments_by_ids(&db, t, &[made])
        .await
        .unwrap();
    assert_eq!(by_id.len(), 1);
    assert_eq!(by_id[0].data, super::png());
}

#[tokio::test]
async fn files_no_reply_took_are_dropped_and_others_threads_are_out_of_reach() {
    let db = store::open_in_memory().await.unwrap();
    let t = store::create_chat_thread(&db, "m", "chat").await.unwrap();
    let other = store::create_chat_thread(&db, "m", "chat").await.unwrap();
    let made = tool_file(&db, t, "c1").await;
    let theirs = tool_file(&db, other, "c9").await;

    assert!(store::chat_attachments_by_ids(&db, t, &[theirs])
        .await
        .unwrap()
        .is_empty());
    // Dropping names this thread's only: the other's stays.
    store::drop_tool_files(&db, t, &[made, theirs])
        .await
        .unwrap();
    assert!(store::chat_attachments_by_ids(&db, t, &[made])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store::chat_attachments_by_ids(&db, other, &[theirs])
            .await
            .unwrap()
            .len(),
        1
    );
}
