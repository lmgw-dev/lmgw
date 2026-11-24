use std::time::Duration;

use super::*;

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

fn key(n: u32) -> Arc<str> {
    Arc::from(format!("key:{n}"))
}

/// A share of `bytes` for `who`, named on `tx` once held, held for `hold`.
fn spawn_hold(
    a: &Answers,
    who: Arc<str>,
    bytes: u64,
    name: &'static str,
    tx: &tokio::sync::mpsc::UnboundedSender<&'static str>,
    hold: Duration,
) -> tokio::task::JoinHandle<()> {
    let (a, tx) = (a.clone(), tx.clone());
    tokio::spawn(async move {
        let s = a.hold(&who, bytes).await.unwrap();
        let _ = tx.send(name);
        tokio::time::sleep(hold).await;
        drop(s);
    })
}

#[tokio::test]
async fn with_one_caller_shares_wait_in_order_once_the_bound_is_held() {
    let a = Answers::new(Some(100));
    a.set_shared(false);
    let first = a.hold(&key(1), 60).await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for (name, bytes) in [("second", 60), ("third", 10)] {
        spawn_hold(&a, key(1), bytes, name, &tx, Duration::from_millis(30));
        settle().await;
    }
    // The small third does not pass the second that waits before it.
    assert!(rx.try_recv().is_err());
    assert_eq!((a.facts().held_bytes, a.facts().waiting), (60, 2));
    drop(first);
    assert_eq!(rx.recv().await, Some("second"));
    assert_eq!(rx.recv().await, Some("third"));
}

/// Security round A review, B2: four jobs of one key at a bound of four
/// shares hold three; another key's small request takes the fourth at once.
#[tokio::test]
async fn one_caller_leaves_a_share_free_and_another_caller_takes_it() {
    let a = Answers::new(Some(400));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let long = Duration::from_secs(3600);
    let hogs: Vec<_> = (0..4)
        .map(|_| spawn_hold(&a, key(1), 100, "hog", &tx, long))
        .collect();
    settle().await;
    let f = a.facts();
    assert_eq!((f.holding, f.waiting), (3, 1), "{f:?}");
    let victim = spawn_hold(&a, key(2), 100, "victim", &tx, Duration::ZERO);
    settle().await;
    let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(got, ["hog", "hog", "hog", "victim"]);
    victim.await.unwrap();
    // The share it gave back stays free for another caller: the hog's
    // fourth waits for one of its own.
    settle().await;
    assert!(rx.try_recv().is_err());
    assert_eq!((a.facts().holding, a.facts().waiting), (3, 1));
    for h in hogs {
        h.abort();
    }
    settle().await;
    assert_eq!(a.facts().holding, 0);
}

/// Review S1 on the answers: two callers at their share, a newcomer, and
/// a share that goes back goes to the newcomer, not to the caller whose
/// share it was.
#[tokio::test]
async fn a_caller_holding_none_goes_before_one_whose_share_just_ended() {
    let a = Answers::new(Some(200));
    let one = a.hold(&key(1), 100).await.unwrap();
    let _two = a.hold(&key(2), 100).await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let long = Duration::from_secs(3600);
    let again = spawn_hold(&a, key(1), 100, "key 1 again", &tx, long);
    settle().await;
    let newcomer = spawn_hold(&a, key(3), 100, "newcomer", &tx, long);
    settle().await;
    assert_eq!(a.facts().waiting, 2);
    drop(one);
    settle().await;
    assert_eq!(rx.try_recv(), Ok("newcomer"));
    assert!(rx.try_recv().is_err());
    again.abort();
    newcomer.abort();
}

/// Review S5: with one caller (the shares off) one key may hold every
/// share, as before the round.
#[tokio::test]
async fn with_the_shares_off_one_caller_holds_every_share() {
    let a = Answers::new(Some(400));
    a.set_shared(false);
    let k = key(1);
    let all: Vec<Share> = futures::future::join_all((0..4).map(|_| a.hold(&k, 100)))
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(a.facts().holding, 4);
    assert!(!a.facts().callers.shared);
    drop(all);
}

#[tokio::test]
async fn a_settled_share_gives_back_what_its_body_does_not_need() {
    let a = Answers::new(Some(100));
    let s = a.hold(&key(1), 100).await.unwrap();
    let waiting = {
        let a = a.clone();
        tokio::spawn(async move { a.hold(&key(2), 90).await.map(|s| s.bytes()) })
    };
    settle().await;
    assert_eq!(a.facts().waiting, 1);
    // The answer was 8 bytes: the waiting share fits beside it now.
    let s = s.settle(8);
    assert_eq!(waiting.await.unwrap(), Ok(90));
    // A body larger than its share is counted, never waited for.
    let s = s.settle(150);
    assert_eq!(a.facts().held_bytes, 150);
    let s = s.grow(10);
    assert_eq!(a.facts().held_bytes, 160);
    drop(s);
    assert_eq!((a.facts().held_bytes, a.facts().holding), (0, 0));
}

#[tokio::test]
async fn a_share_larger_than_the_bound_is_refused_by_name() {
    let a = Answers::new(Some(1 << 30));
    let e = a.hold(&key(1), (1 << 30) + 1).await.err().unwrap();
    assert_eq!((e.limit, e.output), (1 << 30, 512 << 20));
    let m = e.message();
    assert!(m.contains("1.0 GiB") && m.contains("512 MiB"), "{m}");
    assert!(m.contains("answer_memory") && m.contains("MAX_UNCOMPRESSED_BYTES"));
    // Not read here: not bounded.
    let free = Answers::new(None);
    let k = key(1);
    let _many: Vec<Share> =
        futures::future::join_all((0..50).map(|_| free.hold(&k, MAX_UNCOMPRESSED_BYTES)))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
    assert_eq!(free.facts().holding, 50);
}

/// Review S4: below 2 GiB a request's output is half the bound, so a
/// 1.5 GiB machine still converts a small file.
#[test]
fn the_output_budget_is_half_the_bound_where_that_is_less() {
    const GIB: u64 = 1 << 30;
    assert_eq!(Answers::new(None).output_budget(), MAX_UNCOMPRESSED_BYTES);
    assert_eq!(Answers::new(Some(8 * GIB)).output_budget(), GIB);
    assert_eq!(Answers::new(Some(3 * GIB / 2)).output_budget(), 3 * GIB / 4);
    let small = Answers::new(Some(3 * GIB / 2));
    let share =
        super::super::upload::request_share(Some(12), false, 64 << 20, small.output_budget());
    assert!(share <= 3 * GIB / 2, "a 12-byte request fits: {share}");
    assert_eq!(small.facts().output_bytes, 3 * GIB / 4);
}

#[tokio::test]
async fn a_wait_dropped_lets_the_next_in_line() {
    let a = Answers::new(Some(100));
    let first = a.hold(&key(1), 100).await.unwrap();
    let gone = {
        let a = a.clone();
        tokio::spawn(async move { a.hold(&key(2), 100).await.map(|s| s.bytes()) })
    };
    settle().await;
    let next = {
        let a = a.clone();
        tokio::spawn(async move { a.hold(&key(3), 40).await.map(|s| s.bytes()) })
    };
    settle().await;
    gone.abort();
    settle().await;
    drop(first);
    assert_eq!(next.await.unwrap(), Ok(40));
    assert_eq!(a.facts().holding, 0);
}

#[test]
fn the_write_deadline_grows_with_the_body() {
    assert_eq!(write_deadline(0), Duration::from_secs(ANSWER_WRITE_GRACE_S));
    assert_eq!(
        write_deadline(300 << 20),
        Duration::from_secs(ANSWER_WRITE_GRACE_S + 300)
    );
}
