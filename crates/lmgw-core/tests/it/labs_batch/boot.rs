//! A set run written in chunks (labs-store design §3.5): one a crash left
//! half written is not ready, so nothing lists, reads or counts it, and the
//! boot deletes it with its items. And the measurement of Start on a
//! 100,000-row file, ignored in the suite: the route's time, the longest
//! wait of a writer racing it, the store's own chunks, and the database's
//! growth.

use std::time::Instant;

use lmgw_api_types::decision_lab::{BatchStatus, DecisionForm, StateMode};
use lmgw_api_types::lab_store::LabKind;
use lmgw_core::store::{self, NewLabBatch, NewLabItem};
use lmgw_core::vram::nvml::NoTelemetry;
use serde_json::json;
use sqlx::SqlitePool;

use super::*;

/// A set run of `items` items by hand, ready or not as a crash left it.
async fn batch_by_hand(db: &SqlitePool, ready: bool, items: i64) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO lab_batches (lab, preset_snapshot, model, source_name, mapping, item_count,
            concurrency, ready) VALUES ('decision', '{}', 'decide/laya', 'f.csv', '{}', 3, 1, ?1)
         RETURNING id",
    )
    .bind(ready)
    .fetch_one(db)
    .await
    .unwrap();
    for ix in 0..items {
        sqlx::query(
            "INSERT INTO lab_groups (lab, title, form, plan, batch_id, item_ix)
             VALUES ('decision', 't', '{}', '[]', ?1, ?2)",
        )
        .bind(id)
        .bind(ix)
        .execute(db)
        .await
        .unwrap();
    }
    id
}

#[tokio::test]
async fn a_boot_deletes_a_set_run_a_crash_left_half_written() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let (ready, half) = {
        let db = store::open(&data.join("lmgw.sqlite")).await.unwrap();
        // The boot's sweeps stay inside the scratch dir.
        let mut s = store::load_snapshot(&db).await.unwrap().settings;
        s.builds_dir = Some(data.join("builds").display().to_string());
        store::save_settings(&db, &s).await.unwrap();
        let ready = batch_by_hand(&db, true, 3).await;
        let half = batch_by_hand(&db, false, 2).await;

        // Not ready: not listed, not read, not counted, nor its items.
        let listed: Vec<i64> = store::lab_batches(&db, LabKind::Decision)
            .await
            .unwrap()
            .iter()
            .map(|b| b.id)
            .collect();
        assert_eq!(listed, [ready]);
        assert!(store::lab_batch(&db, None, half).await.unwrap().is_none());
        let counts = store::lab_counts(&db, LabKind::Decision).await.unwrap();
        assert_eq!((counts.batches, counts.groups), (1, 3));
        let item: i64 = sqlx::query_scalar("SELECT id FROM lab_groups WHERE batch_id = ?1")
            .bind(half)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(
            store::lab_group(&db, LabKind::Decision, item)
                .await
                .unwrap()
                .is_none(),
            "its items are not the lab's either"
        );
        db.close().await;
        (ready, half)
    };

    let state = AppState::init_with_host(
        data.clone(),
        true,
        Arc::new(RecordingPodman::default()),
        Arc::new(NoTelemetry("test: no GPU".into())),
    )
    .await
    .unwrap();
    let batches: Vec<(i64, bool)> = sqlx::query_as("SELECT id, ready FROM lab_batches")
        .fetch_all(&state.db)
        .await
        .unwrap();
    assert_eq!(batches, [(ready, true)], "the half-written run is gone");
    let items: Vec<i64> = sqlx::query_scalar("SELECT batch_id FROM lab_groups")
        .fetch_all(&state.db)
        .await
        .unwrap();
    assert_eq!(items, [ready; 3], "with its items: {half}'s are gone");
}

/// The database's size in bytes, its WAL checkpointed into it.
async fn db_bytes(db: &SqlitePool) -> i64 {
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(db)
        .await
        .unwrap();
    let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(db)
        .await
        .unwrap();
    let size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(db)
        .await
        .unwrap();
    pages * size
}

/// `n` rows of ticket-like text, about 400 bytes of state each, labelled.
fn big_csv(n: usize) -> String {
    let filler = "My invoice charged me twice for the Pro plan this month, and the \
                  second charge came in after I had already asked support to cancel the \
                  renewal. I have attached the statement. Please refund one of the two \
                  payments and confirm that the plan will not renew again; this is the \
                  second time it happened and I am getting rather annoyed by it.";
    let mut out = String::with_capacity(n * 460);
    out.push_str("id,text,team,angry\n");
    for i in 0..n {
        out.push_str(&format!(
            "T-{i},\"ticket {i}: {filler}\",{},{}\n",
            team_label(i),
            angry_label(i).unwrap_or("")
        ));
    }
    out
}

#[tokio::test]
#[ignore = "a measurement (labs-store design §3.5): \
            cargo nextest run -p lmgw-core --run-ignored only start_of_a_100k --no-capture"]
async fn start_of_a_100k_row_file_measured() {
    const N: usize = 100_000;
    let mut items = Items::new(Duration::ZERO);
    // The job's calls hang: it writes no run while Start is measured.
    items.hang_from = 0;
    items.hang.store(true, Ordering::SeqCst);
    // A file database, where WAL and the write lock behave as in
    // production.
    let dir = tempfile::tempdir().unwrap();
    let db = store::open(&dir.path().join("lmgw.sqlite")).await.unwrap();
    let on_file = AppState::init_for_tests_on(db).await.unwrap();
    let l = batch_lab_on(&items, crate::decision_lab::lab_on(on_file).await).await;
    let (state, gw) = (l.w.state.clone(), l.gw().clone());
    let laya = l.w.name(CPU_ROW);
    let preset = save_set(&gw, "triage", &set(&laya)).await;
    sqlx::query("CREATE TABLE probe (x INTEGER)")
        .execute(&state.db)
        .await
        .unwrap();
    let before = db_bytes(&state.db).await;
    let text = big_csv(N);

    // A writer racing Start: each of its tiny write transactions waits for
    // whatever holds the write lock; its longest wait is the longest any
    // other writer waited.
    let stop = Arc::new(AtomicBool::new(false));
    let probe = {
        let (db, stop) = (state.db.clone(), stop.clone());
        tokio::spawn(async move {
            let (mut longest, mut writes) = (Duration::ZERO, 0u64);
            while !stop.load(Ordering::SeqCst) {
                let t = Instant::now();
                let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
                sqlx::query("INSERT INTO probe (x) VALUES (1)")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                longest = longest.max(t.elapsed());
                writes += 1;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            (longest, writes)
        })
    };
    let t = Instant::now();
    let (status, body) = post(
        &gw,
        "/decision-lab/api/batches",
        json!({"preset_id": preset, "format": "csv", "text": text, "source_name": "big.csv",
               "mapping": {"state": "text", "key": "id",
                           "labels": {"team": "team", "angry": "angry"}}}),
    )
    .await;
    let route = t.elapsed();
    stop.store(true, Ordering::SeqCst);
    let (racing, writes) = probe.await.unwrap();
    assert_eq!(status, 200, "{body}");
    let v: BatchView = serde_json::from_value(body).unwrap();
    assert_eq!((v.total, v.status), (N as u64, BatchStatus::Running));
    let after = db_bytes(&state.db).await;

    // The store's own count of its chunks, on the same items.
    let questions = set(&laya).questions;
    let parsed: Vec<NewLabItem> = (0..N)
        .map(|i| {
            let form = DecisionForm {
                state_mode: StateMode::Text,
                state: format!("ticket {i}: {}", "x".repeat(380)),
                images: vec![],
                questions: questions.clone(),
            };
            NewLabItem {
                title: form.title(),
                form: serde_json::to_string(&form).unwrap(),
                expected: Some(format!(r#"{{"team":"{}"}}"#, team_label(i))),
                key: Some(format!("T-{i}")),
            }
        })
        .collect();
    let t = Instant::now();
    let (_, how) = store::insert_lab_batch(
        &state.db,
        &NewLabBatch {
            lab: LabKind::Decision,
            preset_id: None,
            preset_snapshot: "{}",
            model: &laya,
            source_name: "big.csv",
            mapping: "{}",
            concurrency: 1,
            plan: "[]",
        },
        &parsed,
    )
    .await
    .unwrap()
    .unwrap();
    let stored = t.elapsed();
    println!(
        "Start of {N} rows: {route:?} for the route ({} MiB of CSV); a racing writer's \
         longest wait {racing:?} over {writes} writes; the database grew {:.1} MiB. \
         insert_lab_batch alone: {stored:?} in {} write transactions, the longest {:?}.",
        text.len() >> 20,
        (after - before) as f64 / f64::from(1 << 20),
        how.transactions,
        how.longest
    );
}
