//! [`ToolPool::acquire_many`]: all at once at the caller's turn, clamped to
//! the size in force and the caller's share, nothing held while waiting.

use std::time::Duration;

use super::*;

fn pool_of(slots: u32) -> ToolPool {
    let p = ToolPool::new(Machine {
        cores: 64,
        memory: None,
    });
    p.set_by_hand(slots);
    p
}

/// Let spawned tasks reach their `acquire`.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// A task of `caller` that takes `want` slots, sends how many it got and
/// holds them until `release` fires.
fn many(
    pool: &ToolPool,
    caller: &str,
    want: usize,
    got: tokio::sync::mpsc::UnboundedSender<(&'static str, usize)>,
    name: &'static str,
) -> (tokio::task::JoinHandle<()>, oneshot::Sender<()>) {
    let (release, held) = oneshot::channel::<()>();
    let pool = pool.clone();
    let caller: Caller = caller.into();
    let task = tokio::spawn(as_caller(caller, async move {
        let s = pool.acquire_many(want).await;
        let _ = got.send((name, s.count()));
        let _ = held.await;
    }));
    (task, release)
}

/// A slot of `pool` held by `caller` (another caller than the step under
/// test, so its own share is untouched).
async fn held_by(pool: &ToolPool, caller: &str) -> Slot {
    as_caller(caller, pool.acquire()).await
}

#[tokio::test]
async fn several_slots_are_taken_at_once_up_to_the_share() {
    // One caller takes at most one slot less than the size (`share`).
    for (size, expect) in [(1, 1), (2, 1), (3, 2), (4, 3), (8, 4)] {
        let pool = pool_of(size);
        let s = pool.acquire_many(4).await;
        assert_eq!(s.count(), expect, "size {size}");
        assert_eq!(pool.facts().running as usize, expect);
        drop(s);
        assert_eq!(pool.facts().running, 0);
    }
    // Zero asks for one.
    assert_eq!(pool_of(2).acquire_many(0).await.count(), 1);
}

#[tokio::test]
async fn a_step_waiting_for_several_holds_none_and_takes_them_together() {
    let pool = pool_of(5);
    let one = held_by(&pool, "a").await;
    let two = held_by(&pool, "a").await;
    let three = held_by(&pool, "a").await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_t, _r) = many(&pool, "model", 4, tx, "model");
    settle().await;
    // Two are free, and they stay free: the waiting step holds none.
    let f = pool.facts();
    assert_eq!((f.running, f.waiting), (3, 1));
    drop(one);
    settle().await;
    assert_eq!(pool.facts().running, 2);
    assert!(rx.try_recv().is_err());
    drop(two);
    assert_eq!(rx.recv().await, Some(("model", 4)));
    assert_eq!(pool.facts().running, 5);
    drop(three);
}

#[tokio::test]
async fn a_size_set_while_it_waits_applies_down_and_up() {
    // Down: from 5 to 3 while three slots are held elsewhere; it takes its
    // share of 3 (2) once they are back, instead of waiting for 4 for ever.
    let pool = pool_of(5);
    let held: Vec<Slot> = vec![
        held_by(&pool, "a").await,
        held_by(&pool, "a").await,
        held_by(&pool, "a").await,
    ];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_t, _r) = many(&pool, "model", 4, tx.clone(), "down");
    settle().await;
    pool.set_by_hand(3);
    settle().await;
    assert!(rx.try_recv().is_err(), "3 running of 3");
    drop(held);
    assert_eq!(rx.recv().await, Some(("down", 2)));
    assert_eq!(pool.facts().running, 2);

    // Up: from 3 to 4 while two are held; it takes its share of 4 (3) once
    // they are back.
    let pool = pool_of(3);
    let held = as_caller("a", pool.acquire_many(2)).await;
    assert_eq!(held.count(), 2);
    let (_t, _r) = many(&pool, "model", 4, tx, "up");
    settle().await;
    pool.set_by_hand(4);
    settle().await;
    assert!(rx.try_recv().is_err());
    drop(held);
    assert_eq!(rx.recv().await, Some(("up", 3)));
    // A size set to fit what is free hands it out at once.
    let pool = pool_of(4);
    let one = held_by(&pool, "a").await;
    let two = held_by(&pool, "a").await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_t, _r) = many(&pool, "model", 4, tx, "fits");
    settle().await;
    assert!(rx.try_recv().is_err(), "3 wanted, 2 free");
    pool.set_by_hand(8);
    assert_eq!(rx.recv().await, Some(("fits", 4)));
    drop((one, two));
}

#[tokio::test]
async fn a_step_waiting_for_several_keeps_its_turn_against_a_stream_of_single_steps() {
    let pool = pool_of(4);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // Another caller holds two slots and keeps queueing single steps.
    let (_a0, r0) = many(&pool, "batch", 1, tx.clone(), "batch-0");
    assert_eq!(rx.recv().await, Some(("batch-0", 1)));
    let (_a1, r1) = many(&pool, "batch", 1, tx.clone(), "batch-1");
    assert_eq!(rx.recv().await, Some(("batch-1", 1)));
    let (_m, rm) = many(&pool, "model", 4, tx.clone(), "model");
    settle().await;
    let (_a2, r2) = many(&pool, "batch", 1, tx.clone(), "batch-2");
    settle().await;
    // Two slots are free, but the model's turn came first: batch-2 waits.
    assert!(rx.try_recv().is_err());
    assert_eq!(pool.facts().waiting, 2);
    let _ = r0.send(());
    assert_eq!(rx.recv().await, Some(("model", 3)));
    let _ = rm.send(());
    assert_eq!(rx.recv().await, Some(("batch-2", 1)));
    let _ = (r1.send(()), r2.send(()));
}

#[tokio::test]
async fn a_step_that_goes_away_while_waiting_for_several_lets_the_others_on() {
    let pool = pool_of(3);
    let one = held_by(&pool, "a").await;
    let two = held_by(&pool, "a").await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (gone, _r) = many(&pool, "model", 3, tx.clone(), "gone");
    settle().await;
    let (_t, _r2) = many(&pool, "other", 1, tx, "next");
    settle().await;
    assert!(rx.try_recv().is_err(), "behind the model's turn");
    gone.abort();
    // The free slot goes on at once, not at the next slot given back.
    assert_eq!(rx.recv().await, Some(("next", 1)));
    let f = pool.facts();
    assert_eq!((f.running, f.waiting), (3, 0));
    drop((one, two));
}

#[tokio::test(start_paused = true)]
async fn waiting_for_a_slot_does_not_count_on_the_file_s_clock() {
    let pool = pool_of(1);
    let held = pool.acquire().await;
    let clock = clock::FileClock::new(Duration::from_secs(60));
    let p = pool.clone();
    let task = tokio::spawn(clock::scope(clock.clone(), async move {
        let s = p.acquire().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        // A second wait, behind the slot this step still waits for.
        drop(s);
        let _s = p.acquire_many(1).await;
    }));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(clock.used(), Duration::ZERO, "waiting for the first slot");
    drop(held);
    task.await.unwrap();
    assert_eq!(clock.used(), Duration::from_secs(2));
}
