//! The storage count's reservations (security round A verification, N-a).

use super::*;
use lmgw_api_types::documents::convert::{ConvertedDocument, ErrorCode};

/// A file's entry holding `n` bytes of text.
fn entry(n: usize) -> ConvertedDocument {
    let mut d = crate::extract::convert::batch::failed_file(
        0,
        "example.md".into(),
        ErrorCode::Unreadable,
        String::new(),
    )
    .doc;
    d.content = Some("x".repeat(n));
    d
}

/// A result write dropped midway (its job cancelled while the entry was
/// written) gives back its reservation as it is dropped, not at the hourly
/// resync; a written one keeps it, as what is on disk.
#[tokio::test]
async fn a_write_dropped_by_a_cancel_gives_its_reservation_back_at_once() {
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let ledger = &state.convert_jobs.ledger;
    ledger.ready(&state).await;
    let before = ledger.stored();
    let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
    let mut tx = Some(tx);
    let r = Reservation::new(&state);
    {
        let write = results::write_within(
            &state.data_dir,
            "cj_0e0a",
            entry(4 << 20),
            |n| {
                let ok = r.room(n);
                if let Some(t) = tx.take() {
                    let _ = t.send(n);
                }
                ok
            },
            &r.landed,
        );
        // The write runs until its room is taken, then is dropped at its
        // next wait, as the job's `select!` drops it on a cancel.
        tokio::select! {
            biased;
            _ = write => panic!("the write ended before it could be dropped"),
            n = rx => {
                let n = n.unwrap();
                assert!(n > 4 << 20);
                assert_eq!(ledger.stored(), before + n, "reserved while it writes");
            }
        }
    }
    assert_eq!(r.landed.bytes(), 0, "nothing finished on disk");
    drop(r);
    assert_eq!(ledger.stored(), before, "given back as it was dropped");

    // Written whole, the reservation stays: it is what the disk holds.
    let r = Reservation::new(&state);
    let size = results::write_within(
        &state.data_dir,
        "cj_0e0b",
        entry(1 << 10),
        |n| r.room(n),
        &r.landed,
    )
    .await
    .unwrap();
    assert_eq!(r.landed.bytes(), size);
    drop(r);
    assert_eq!(ledger.stored(), before + size);
    let gone = results::remove(&state.data_dir, "cj_0e0b").await;
    ledger.sub(gone);
    results::remove(&state.data_dir, "cj_0e0a").await;
    assert_eq!(ledger.stored(), before);
}
