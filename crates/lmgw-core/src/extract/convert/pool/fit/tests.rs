//! A slot fitted to what its step holds (`Slot::fit`, review of the poppler
//! limit, B1): two geometry outputs of 1 GiB need about 6 GiB to be read,
//! more than a PDF's runs are charged (`charge::PDF_FILE`, 2.5 GiB).

use lmgw_api_types::documents::MemorySource;

use super::super::pool_size::{Machine, Memory};
use super::super::{as_caller, as_caller_now, charge, ToolPool};

const GIB: u64 = 1 << 30;

fn pool(gib: u64) -> ToolPool {
    ToolPool::new(Machine {
        cores: 8,
        memory: Some(Memory {
            bytes: gib * GIB,
            source: MemorySource::Physical,
        }),
    })
}

#[tokio::test]
async fn a_slot_grows_to_what_two_1_gib_outputs_need_or_is_given_back() {
    let p = pool(8);
    // One caller (shares off): its share does not bound the grow here.
    p.set_shared(false);
    // Zeroed pages are not touched: two 1 GiB outputs cost nothing here.
    let outputs = (vec![0u8; 1 << 30], vec![0u8; 1 << 30]);
    let need = charge::after_runs(&[&outputs.0, &outputs.1]);
    assert!(need >= 6 * GIB, "{need}");
    // Room in the pool: grown at once, charged all of it (another step
    // runs, so not by the alone rule).
    let beside = as_caller_now("beside.example", || p.try_acquire_charged(1, 1)).unwrap();
    let slot = p.acquire_charged(1, charge::PDF_FILE).await;
    let slot = slot
        .fit(need)
        .unwrap_or_else(|_| panic!("an empty pool grows it"));
    assert_eq!((slot.bytes(), p.facts().charged_bytes), (need, need + 1));
    drop((slot, beside));
    assert_eq!(p.facts().charged_bytes, 0);
    // Another PDF holds 2.5 GiB: no room now, and nothing waits for it;
    // the slot comes back as it was, for its step to give back.
    let other = as_caller_now("other.example", || {
        p.try_acquire_charged(1, charge::PDF_FILE)
    })
    .unwrap();
    let slot = p.acquire_charged(1, charge::PDF_FILE).await;
    let Err(slot) = slot.fit(need) else {
        panic!("grown past the pool's memory");
    };
    assert_eq!(slot.bytes(), charge::PDF_FILE);
    assert_eq!(p.facts().charged_bytes, 2 * charge::PDF_FILE);
    drop(slot);
    // Given back, the step's new slot is charged what the outputs need
    // once the other has ended: never below it.
    drop(other);
    let again = p.acquire_charged(1, need.max(charge::PDF_FILE)).await;
    assert_eq!(p.facts().charged_bytes, need);
    // Less than it holds: shrunk to it, as before.
    let small = again
        .fit(GIB)
        .unwrap_or_else(|_| panic!("a shrink never fails"));
    assert_eq!(p.facts().charged_bytes, GIB);
    drop(small);
}

/// A slot whose tool is still being reaped (a lease of it lives) keeps its
/// charge and is not grown.
#[tokio::test]
async fn a_leased_slot_is_not_grown() {
    let p = pool(16);
    let slot = p.acquire_charged(1, charge::PDF_FILE).await;
    let lease = slot.lease();
    let Err(slot) = slot.fit(4 * GIB) else {
        panic!("a leased slot grew");
    };
    assert_eq!(p.facts().charged_bytes, charge::PDF_FILE);
    drop((slot, lease));
    assert_eq!(p.facts().charged_bytes, 0);
}

/// Review 2 of the poppler limit, S3: a grow never passes a step that
/// waits, nor the caller's share (one more step like its own must still fit
/// beside its charges); it is refused, and the step takes its place in the
/// queue with a new slot.
#[tokio::test]
async fn a_grow_passes_neither_the_queue_nor_the_caller_s_share() {
    let p = pool(8);
    p.set_shared(false);
    // Another caller's step runs (so no grow here is the only step); a
    // step of a third waits for 6 GiB.
    let other = as_caller_now("other.example", || p.try_acquire_charged(1, 1)).unwrap();
    let slot = as_caller("a.example", p.acquire_charged(1, charge::PDF_FILE)).await;
    let waiter = {
        let p = p.clone();
        tokio::spawn(as_caller("w.example", async move {
            drop(p.acquire_charged(1, 6 * GIB).await)
        }))
    };
    while p.facts().waiting == 0 {
        tokio::task::yield_now().await;
    }
    // 2.5 + 0.5 fits the 8 GiB, but a step waits.
    let Err(slot) = slot.fit(3 * GIB) else {
        panic!("grown past a waiting step");
    };
    assert_eq!(slot.bytes(), charge::PDF_FILE);
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(p.facts().waiting, 0);
    // Nobody waits: the same grow fits now.
    let slot = slot
        .fit(3 * GIB)
        .unwrap_or_else(|_| panic!("refused with nobody waiting"));
    drop(slot);
    // Shares on: the caller's 2.5 GiB grown by 2 GiB leaves 3.5 GiB free,
    // one more step like its own first charge (2.5 GiB) beside it. Grown
    // again by 1.5 GiB the memory still fits (7 GiB of 8), but its 6 GiB
    // would leave 2 GiB, less than one more step like its 4.5 GiB.
    p.set_shared(true);
    let slot = as_caller("a.example", p.acquire_charged(1, charge::PDF_FILE)).await;
    let slot = slot
        .fit(charge::PDF_FILE + 2 * GIB)
        .unwrap_or_else(|_| panic!("the share allows this grow"));
    let Err(slot) = slot.fit(charge::PDF_FILE + 2 * GIB + GIB + GIB / 2) else {
        panic!("grown past the caller's share");
    };
    assert_eq!(slot.bytes(), charge::PDF_FILE + 2 * GIB);
    drop((slot, other));
    assert_eq!(p.facts().charged_bytes, 0);
}

/// Review 2, nits: the only step running grows past the memory (a step
/// heavier than all of it runs alone, as it is admitted), and a grown pair
/// keeps one slot.
#[tokio::test]
async fn the_only_step_grows_alone_and_a_grown_pair_keeps_one_slot() {
    let p = pool(4);
    let pair = p.acquire_charged(2, 2 * charge::POPPLER_TEXT_RUN).await;
    assert_eq!(pair.count(), 2);
    let grown = pair
        .fit(6 * GIB)
        .unwrap_or_else(|_| panic!("the only step grows alone"));
    let f = p.facts();
    assert_eq!((grown.count(), f.running, f.charged_bytes), (1, 1, 6 * GIB));
    drop(grown);
    assert_eq!((p.facts().running, p.facts().charged_bytes), (0, 0));
}
