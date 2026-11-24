use std::time::Duration;

use super::*;

fn machine(cores: u32) -> Machine {
    Machine {
        cores,
        memory: None,
    }
}

/// Start a step of `caller` that takes a slot from `pool`, says its name on
/// `done` once it has it, and holds it until `release` fires.
fn step(
    pool: &ToolPool,
    caller: impl Into<Caller>,
    name: &'static str,
    done: tokio::sync::mpsc::UnboundedSender<&'static str>,
) -> (tokio::task::JoinHandle<()>, oneshot::Sender<()>) {
    let (release, held) = oneshot::channel::<()>();
    let pool = pool.clone();
    let caller: Caller = caller.into();
    let task = tokio::spawn(as_caller(caller, async move {
        let _slot = pool.acquire().await;
        let _ = done.send(name);
        let _ = held.await;
    }));
    (task, release)
}

/// Let the spawned steps reach their `acquire`.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn a_free_slot_goes_round_robin_over_the_waiting_callers() {
    let pool = ToolPool::new(machine(1));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_t0, first) = step(&pool, "batch", "batch-0", tx.clone());
    settle().await;
    assert_eq!(rx.recv().await, Some("batch-0"));
    // A batch of three queues, then a one-page caller after it.
    let mut releases = Vec::new();
    for name in ["batch-1", "batch-2", "batch-3"] {
        releases.push(step(&pool, "batch", name, tx.clone()));
        settle().await;
    }
    let (_t, single) = step(&pool, "single", "single-0", tx.clone());
    settle().await;
    assert_eq!(pool.facts().waiting, 4);
    let _ = first.send(());
    // The single caller's step comes first, not after the whole batch: it
    // holds nothing and was never served, the batch's step just ended
    // (security round A review S1).
    let mut order = Vec::new();
    order.push(rx.recv().await.unwrap());
    let _ = single.send(());
    order.push(rx.recv().await.unwrap());
    let mut releases = releases.into_iter();
    let (_t1, r1) = releases.next().unwrap();
    let _ = r1.send(());
    order.push(rx.recv().await.unwrap());
    let (_t2, r2) = releases.next().unwrap();
    let _ = r2.send(());
    order.push(rx.recv().await.unwrap());
    assert_eq!(order, ["single-0", "batch-1", "batch-2", "batch-3"]);
}

#[tokio::test]
async fn a_step_that_goes_away_while_waiting_gives_its_turn_on() {
    let pool = ToolPool::new(machine(1));
    let held = pool.acquire().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (gone, _r) = step(&pool, "a", "gone", tx.clone());
    settle().await;
    let (_t, _r2) = step(&pool, "b", "next", tx.clone());
    settle().await;
    gone.abort();
    settle().await;
    drop(held);
    assert_eq!(rx.recv().await, Some("next"));
    assert_eq!(pool.facts().running, 1);
}

#[tokio::test]
async fn the_size_set_by_hand_wins_and_zero_goes_back_to_derived() {
    let pool = ToolPool::new(machine(2));
    assert_eq!(pool.facts().slots, 2);
    assert!(!pool.facts().by_hand);
    // Two callers: one alone holds at most one slot of two (`share`).
    let a = pool.acquire().await;
    let b = as_caller("b", pool.acquire()).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_t, _r) = step(&pool, "a", "third", tx);
    settle().await;
    assert_eq!(pool.facts().waiting, 1);
    // A larger size by hand hands the waiting step its slot at once.
    pool.set_by_hand(3);
    assert_eq!(rx.recv().await, Some("third"));
    let f = pool.facts();
    assert_eq!((f.slots, f.derived, f.by_hand, f.running), (3, 2, true, 3));
    pool.set_by_hand(0);
    assert_eq!(pool.facts().slots, 2);
    drop((a, b));
}

#[tokio::test]
async fn a_blocking_step_runs_with_a_slot_of_the_global_pool() {
    let n = blocking(|| 40 + 2).await.unwrap();
    assert_eq!(n, 42);
}

/// A slot handed to a waiter whose future is dropped before it polls again
/// goes back to the pool (`Wait::drop`), and the waiter stops counting.
#[tokio::test]
async fn a_slot_handed_to_a_dropped_waiter_is_released() {
    let pool = ToolPool::new(machine(1));
    let held = pool.acquire().await;
    let mut waiter = Box::pin(pool.acquire());
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    assert_eq!(pool.facts().waiting, 1);
    // The holder lets go: the slot is handed to the waiter's channel ...
    drop(held);
    assert_eq!(pool.facts().running, 1);
    // ... and the waiter is dropped before it took it.
    drop(waiter);
    let f = pool.facts();
    assert_eq!((f.running, f.waiting), (0, 0));
    let again = tokio::time::timeout(Duration::from_secs(5), pool.acquire())
        .await
        .expect("the pool still holds a slot nobody uses");
    assert_eq!(pool.facts().running, 1);
    drop(again);
}

#[tokio::test]
async fn a_waiter_that_goes_away_stops_being_counted_at_once() {
    let pool = ToolPool::new(machine(1));
    let _held = pool.acquire().await;
    let mut waiter = Box::pin(pool.acquire());
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    assert_eq!(pool.facts().waiting, 1);
    drop(waiter);
    assert_eq!(pool.facts().waiting, 0);
}

/// Two callers without a key each have a caller of their own: the one-step
/// request is served after at most one of the batch's steps.
#[tokio::test]
async fn keyless_requests_do_not_share_a_queue() {
    assert_ne!(request_caller(), request_caller());
    let pool = ToolPool::new(machine(1));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (batch, single) = (request_caller(), request_caller());
    let (_t0, first) = step(&pool, batch.clone(), "batch-0", tx.clone());
    settle().await;
    assert_eq!(rx.recv().await, Some("batch-0"));
    let mut rest = Vec::new();
    for name in ["batch-1", "batch-2", "batch-3", "batch-4"] {
        rest.push(step(&pool, batch.clone(), name, tx.clone()));
        settle().await;
    }
    let (_ts, one) = step(&pool, single.clone(), "single-0", tx.clone());
    settle().await;
    let _ = first.send(());
    // At most one of the batch's steps comes first.
    let mut order = vec![rx.recv().await.unwrap()];
    let mut rest = rest.into_iter();
    let (_t, release) = rest.next().unwrap();
    let _ = release.send(());
    order.push(rx.recv().await.unwrap());
    assert_eq!(order, ["batch-1", "single-0"]);
    drop((one, rest));
}

/// A panic in a blocking step is that step's error, and its slot is given
/// back.
#[tokio::test]
async fn a_panicking_blocking_step_gives_its_slot_back() {
    let pool = ToolPool::new(machine(1));
    let r = blocking_in(&pool, || -> u32 { panic!("a reader bug") }).await;
    assert!(r.unwrap_err().is_panic());
    assert_eq!(pool.facts().running, 0);
    assert_eq!(blocking_in(&pool, || 7).await.unwrap(), 7);
}
