//! The memory a step is charged (`charge.rs`): steps run while their
//! charges fit the memory, a heavy step waits its turn without being
//! passed, a step heavier than all of it runs alone, the charge goes back
//! with the last lease, and a size set by hand counts no memory.

use std::time::Duration;

use super::pool_size::Memory;
use super::*;
use lmgw_api_types::documents::MemorySource;

const GIB: u64 = 1 << 30;

fn pool(cores: u32, gib: u64) -> ToolPool {
    ToolPool::new(Machine {
        cores,
        memory: Some(Memory {
            bytes: gib * GIB,
            source: MemorySource::Physical,
        }),
    })
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn steps_run_while_their_charges_fit_the_memory() {
    // A 16 GiB, 8-core machine: seven pages of text recognition at once,
    // six PDFs (four before poppler's limits were measured, 2026-11-20).
    // Each by a caller of its own: one caller alone leaves a step's worth
    // (`share`, the test below).
    let p = pool(8, 16);
    let by = |i: usize, bytes| {
        as_caller_now(format!("k{i}").as_str(), || p.try_acquire_charged(1, bytes))
    };
    let pages: Vec<Slot> = (0..7).map(|i| by(i, charge::OCR_PAGE).unwrap()).collect();
    assert!(by(7, charge::OCR_PAGE).is_none());
    assert_eq!(p.facts().charged_bytes, 7 * charge::OCR_PAGE);
    drop(pages);
    let pdfs: Vec<Slot> = (0..6).map(|i| by(i, charge::PDF_FILE).unwrap()).collect();
    assert!(by(6, charge::PDF_FILE).is_none());
    drop(pdfs);
    let f = p.facts();
    assert_eq!((f.slots, f.charged_bytes, f.running), (8, 0, 0));
    let c = f.derived_from.charges;
    assert_eq!(
        (
            c.ocr_page.fit,
            c.pdf_file.fit,
            c.reader_run.fit,
            c.page_crops.fit
        ),
        (7, 6, 3, 2)
    );
    assert_eq!(
        (
            c.ocr_page.per_caller,
            c.pdf_file.per_caller,
            c.reader_run.per_caller
        ),
        (6, 5, 2)
    );
    assert_eq!(f.per_caller_slots, 7);
}

#[tokio::test]
async fn one_caller_leaves_a_step_of_memory_and_a_slot_for_another() {
    let p = pool(8, 16);
    let a = |bytes| as_caller_now("a", || p.try_acquire_charged(1, bytes));
    // Six pages of one caller fit, the seventh would leave no page free.
    let pages: Vec<Slot> = (0..6).map(|_| a(charge::OCR_PAGE).unwrap()).collect();
    assert!(a(charge::OCR_PAGE).is_none());
    // Another caller's page starts beside them.
    let b = as_caller_now("b", || p.try_acquire_charged(1, charge::OCR_PAGE));
    assert!(b.is_some());
    drop((pages, b));
    // By slots, memory not counted: seven of eight.
    p.set_by_hand(8);
    let many: Vec<Slot> = (0..7).map(|_| a(charge::PDF_FILE).unwrap()).collect();
    assert!(a(1).is_none());
    assert!(as_caller_now("b", || p.try_acquire_charged(1, 1)).is_some());
    drop(many);
}

#[tokio::test]
async fn a_heavy_step_keeps_its_turn_and_one_heavier_than_all_runs_alone() {
    let p = pool(8, 8);
    let first = p.acquire_charged(1, 6 * GIB).await;
    // The next heavy step waits; a light one behind it does not pass it.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for (name, bytes) in [("heavy", 6 * GIB), ("light", GIB)] {
        let (p, tx) = (p.clone(), tx.clone());
        tokio::spawn(async move {
            let s = p.acquire_charged(1, bytes).await;
            let _ = tx.send(name);
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(s);
        });
        settle().await;
    }
    assert!(rx.try_recv().is_err());
    drop(first);
    let mut got = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    got.sort_unstable();
    assert_eq!(got, ["heavy", "light"]);
    // Heavier than all the memory: alone, but it runs.
    let huge = p.acquire_charged(1, 100 * GIB).await;
    assert!(p.try_acquire_charged(1, 1).is_none());
    drop(huge);
}

#[tokio::test]
async fn the_charge_goes_back_with_the_last_lease() {
    let p = pool(4, 8);
    let slot = p.acquire_charged(1, 6 * GIB).await;
    let lease = slot.lease();
    drop(slot);
    assert!(p.try_acquire_charged(1, 6 * GIB).is_none());
    drop(lease);
    assert!(p.try_acquire_charged(1, 6 * GIB).is_some());
}

#[tokio::test]
async fn a_size_set_by_hand_counts_no_memory() {
    let p = pool(8, 8);
    p.set_by_hand(3);
    let held: Vec<Slot> = (0..3)
        .map(|i| {
            as_caller_now(format!("k{i}").as_str(), || {
                p.try_acquire_charged(1, charge::PDF_FILE)
            })
            .unwrap()
        })
        .collect();
    assert!(
        p.try_acquire_charged(1, 1).is_none(),
        "the size still holds"
    );
    let f = p.facts();
    assert!(f.by_hand && !f.memory_counted);
    assert_eq!(f.derived_from.charges.pdf_file.fit, 3);
    drop(held);
}

#[tokio::test]
async fn a_slot_shrinks_once_no_lease_of_it_lives() {
    let p = pool(8, 16);
    let two = p.try_acquire_charged(2, 10 * GIB).unwrap();
    let lease = two.lease();
    // A lease lives (a tool not reaped yet): nothing goes back.
    let two = two.shrink(1, 6 * GIB);
    assert_eq!((two.count(), p.facts().charged_bytes), (2, 10 * GIB));
    drop(lease);
    let one = two.shrink(1, 6 * GIB);
    let f = p.facts();
    assert_eq!((one.count(), f.running, f.charged_bytes), (1, 1, 6 * GIB));
    drop(one);
    assert_eq!(p.facts().running, 0);
}

/// PDF charge after the runs (2026-11-20): on an 8 GiB machine three PDFs'
/// runs fit at a time (2.5 GiB each, poppler's limits from measured
/// peaks); once the first PDF's runs have ended and its slot holds only its
/// outputs and their reading, the next PDF's runs start beside its layout
/// pass. On 16 GiB the same for the seventh.
#[tokio::test]
async fn the_next_pdf_starts_once_the_first_s_runs_have_ended() {
    let outputs = vec![b'x'; 300 << 10];
    let reading = charge::after_runs(&[&outputs]);
    for (gib, fit) in [(8, 3), (16, 6)] {
        let p = pool(8, gib);
        // One caller (sign-in off), as the measurement's batch.
        p.set_shared(false);
        let mut first: Vec<Slot> = Vec::new();
        for _ in 0..fit {
            first.push(p.acquire_charged(1, charge::PDF_FILE).await);
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let next = {
            let p = p.clone();
            tokio::spawn(async move {
                let s = p.acquire_charged(1, charge::PDF_FILE).await;
                let _ = tx.send(());
                s
            })
        };
        settle().await;
        assert!(rx.try_recv().is_err(), "{gib} GiB: the next PDF waits");
        let shrunk = first.remove(0).shrink(1, reading);
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("admitted once the runs ended")
            .unwrap();
        let next = next.await.unwrap();
        let f = p.facts();
        assert_eq!(f.running, fit + 1, "{gib} GiB");
        assert_eq!(
            f.charged_bytes,
            fit as u64 * charge::PDF_FILE + reading,
            "{gib} GiB"
        );
        drop((shrunk, first, next));
        assert_eq!(p.facts().charged_bytes, 0);
    }
}

/// A shrunk slot gives back exactly the difference, keeps its slot, and
/// never grows: a charge above what it holds keeps what it holds.
#[tokio::test]
async fn a_shrunk_slot_gives_back_exactly_the_difference() {
    let p = pool(8, 16);
    let other = p.acquire_charged(1, GIB).await;
    let slot = p.acquire_charged(1, charge::PDF_FILE).await;
    let before = p.facts().charged_bytes;
    let outputs = (vec![0u8; 5 << 20], Vec::with_capacity(3 << 20));
    let held = charge::after_runs(&[&outputs.0, &outputs.1]);
    assert_eq!(
        held,
        (5 << 20) + (3 << 20) + charge::READ_PER_OUTPUT_BYTE * (5 << 20)
    );
    let slot = slot.shrink(1, held);
    let f = p.facts();
    assert_eq!(before - f.charged_bytes, charge::PDF_FILE - held);
    assert_eq!((f.running, f.charged_bytes), (2, GIB + held));
    let slot = slot.shrink(1, charge::PDF_FILE);
    assert_eq!(p.facts().charged_bytes, GIB + held);
    drop(slot);
    assert_eq!(p.facts().charged_bytes, GIB);
    drop(other);
}
