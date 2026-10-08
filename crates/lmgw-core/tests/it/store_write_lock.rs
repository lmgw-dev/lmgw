//! Every write transaction takes the write lock at its BEGIN
//! (`store::begin_write`), on a file database in WAL mode as the gateway
//! runs it.

use lmgw_core::store::{self, ChatReply};

/// Writers side by side, each reading a reply and then writing it back in
/// one transaction (`continue_chat_reply`, the save of a continue) while
/// the others commit theirs. A deferred transaction reads at a snapshot,
/// and a commit by another connection after that read fails its first
/// write at once with `SQLITE_BUSY_SNAPSHOT` ("database is locked"): the
/// busy timeout does not apply to it. One that takes the write lock at its
/// BEGIN waits for the lock instead, and none fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn read_then_write_transactions_side_by_side_never_fail_as_locked() {
    const WRITERS: usize = 8;
    const ROUNDS: usize = 20;
    let dir = tempfile::tempdir().unwrap();
    let db = store::open(&dir.path().join("lmgw.sqlite")).await.unwrap();
    let thread = store::create_chat_thread(&db, "m", "chat").await.unwrap();
    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let reply = ChatReply {
            content: format!("reply {w}"),
            ..ChatReply::default()
        };
        let id = store::append_chat_reply(&db, thread, &reply).await.unwrap();
        let db = db.clone();
        writers.push(tokio::spawn(async move {
            let mut failed = Vec::new();
            for _ in 0..ROUNDS {
                match store::continue_chat_reply(&db, thread, id, &reply.content, &reply).await {
                    Ok(store::ContinueSave::Saved) => {}
                    Ok(other) => failed.push(format!("{other:?}")),
                    Err(e) => failed.push(e.to_string()),
                }
            }
            failed
        }));
    }
    let mut failed = Vec::new();
    for w in writers {
        failed.extend(w.await.unwrap());
    }
    assert!(
        failed.is_empty(),
        "{} of {} read-then-write transactions failed: {:?}",
        failed.len(),
        WRITERS * ROUNDS,
        failed.iter().take(3).collect::<Vec<_>>()
    );
}
