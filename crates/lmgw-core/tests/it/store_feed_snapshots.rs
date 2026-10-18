//! The Chat feed's two snapshot reads (`store::feed::bounds_and_switch`,
//! `store::feed::page_with_bounds`) begin deferred on purpose: they only
//! read, for one consistent snapshot, which is clippy.toml's one allowed use
//! of a deferred BEGIN. On the WAL file database the gateway runs, a deferred
//! transaction fails with "database is locked" only when it *writes* after
//! another connection committed past its snapshot (`SQLITE_BUSY_SNAPSHOT`):
//! the control below shows this harness produces exactly that, and the race
//! shows the two reads never meet it.

use lmgw_core::store::{self, feed};

/// The hazard `store::begin_write` exists for, made to happen: a deferred
/// transaction reads, another connection commits, and the first one's write
/// is refused at once ("database is locked"), busy timeout or not. Without
/// this the race below could pass for a reason that has nothing to do with
/// what it claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_a_deferred_read_then_write_is_refused_after_another_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = store::open(&dir.path().join("lmgw.sqlite")).await.unwrap();
    store::create_chat_thread(&db, "m", "chat").await.unwrap();
    // The control is the deferred transaction that writes: what nothing
    // outside a test may begin.
    #[allow(clippy::disallowed_methods)]
    let mut tx = db.begin().await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_threads")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(before, 1);
    // Another connection commits past the snapshot the read just took.
    store::create_chat_thread(&db, "m", "chat").await.unwrap();
    let err = sqlx::query("INSERT INTO chat_threads (model_alias, kind) VALUES ('m', 'chat')")
        .execute(&mut *tx)
        .await
        .expect_err("a deferred transaction's write after a newer commit is refused");
    assert!(
        err.to_string().contains("database is locked"),
        "the refusal is SQLITE_BUSY_SNAPSHOT: {err}"
    );
}

/// Writers commit Chat feed records (a thread's creation writes one) side by
/// side with readers running both snapshot reads in a loop. No read fails,
/// and each one's bounds and page agree: the page never holds a record past
/// the head read beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_feed_snapshot_reads_never_fail_beside_committing_writers() {
    const WRITERS: usize = 4;
    const THREADS_EACH: usize = 40;
    const READERS: usize = 4;
    const READS_EACH: usize = 100;
    let dir = tempfile::tempdir().unwrap();
    let db = store::open(&dir.path().join("lmgw.sqlite")).await.unwrap();

    let mut writers = Vec::new();
    for _ in 0..WRITERS {
        let db = db.clone();
        writers.push(tokio::spawn(async move {
            for _ in 0..THREADS_EACH {
                store::create_chat_thread(&db, "m", "chat").await.unwrap();
            }
        }));
    }
    let mut readers = Vec::new();
    for r in 0..READERS {
        let db = db.clone();
        readers.push(tokio::spawn(async move {
            let mut failed = Vec::new();
            for i in 0..READS_EACH {
                // A device key that does not exist reads as `off`; the read
                // of its row is what matters here, not the level.
                let key = (i % 2 == 0).then_some(r as i64 + 1);
                if let Err(e) = feed::bounds_and_switch(&db, key).await {
                    failed.push(format!("bounds_and_switch: {e}"));
                }
                match feed::page_with_bounds(&db, 0, 1000).await {
                    Ok((records, bounds)) => {
                        if let Some(past) = records.iter().find(|rec| rec.seq > bounds.head) {
                            failed.push(format!(
                                "page_with_bounds: record {} past the head {} read with it",
                                past.seq, bounds.head
                            ));
                        }
                    }
                    Err(e) => failed.push(format!("page_with_bounds: {e}")),
                }
            }
            failed
        }));
    }
    for w in writers {
        w.await.unwrap();
    }
    let mut failed = Vec::new();
    for r in readers {
        failed.extend(r.await.unwrap());
    }
    assert!(
        failed.is_empty(),
        "{} of {} snapshot reads failed: {:?}",
        failed.len(),
        READERS * READS_EACH * 2,
        failed.iter().take(3).collect::<Vec<_>>()
    );
    let (_, bounds) = feed::page_with_bounds(&db, 0, 1).await.unwrap();
    assert!(
        bounds.head >= (WRITERS * THREADS_EACH) as i64,
        "every writer's records are in: head {}",
        bounds.head
    );
}
