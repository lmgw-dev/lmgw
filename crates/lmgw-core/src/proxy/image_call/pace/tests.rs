use std::time::Duration;

use tokio::time::Instant;

use serde_json::json;

use super::*;

fn at(t0: Instant, ms: u64) -> Instant {
    t0 + Duration::from_millis(ms)
}

fn step(pass: u32, step: u32) -> Value {
    json!({"status": "generating", "queue_position": 0,
           "preview": {"pass": pass, "step": step, "total_steps": 8}})
}

#[test]
fn a_job_is_judged_only_once_it_has_stepped_twice() {
    let t0 = Instant::now();
    let mut p = Pace::new(t0);
    p.read(&json!({"status": "queued", "queue_position": 1}), at(t0, 0));
    // A long queue: no pace, however long it stands still.
    assert_eq!(p.left(at(t0, 600_000)), Ok(None));
    p.read(&json!({"status": "generating"}), at(t0, 30_000));
    p.read(&step(1, 1), at(t0, 31_000));
    assert_eq!(p.left(at(t0, 900_000)), Ok(None), "one step says nothing");
    p.read(&step(1, 2), at(t0, 31_500));
    assert!(p.judged());
    // The slowest gap is the text encoder's second before step 1, not the
    // 30 s queue: twenty of it from the last advance.
    assert_eq!(p.left(at(t0, 31_500)), Ok(Some(Duration::from_secs(20))));
    assert_eq!(
        p.left(at(t0, 41_500)),
        Ok(Some(Duration::from_secs(10))),
        "half-way"
    );
    let stall = p.left(at(t0, 51_500)).unwrap_err();
    assert_eq!(stall.quiet, Duration::from_secs(20));
    assert_eq!(stall.slowest, Duration::from_secs(1));
    assert_eq!(
        stall.to_string(),
        "made no progress for 20.0 s, 20 times its slowest step so far (1.0 s), a pace judged \
         once a job has taken 2 steps"
    );
}

#[test]
fn the_same_read_again_is_no_advance_and_a_new_step_restarts_the_clock() {
    let t0 = Instant::now();
    let mut p = Pace::new(t0);
    p.read(&step(1, 1), at(t0, 0));
    p.read(&step(1, 2), at(t0, 250));
    p.read(&step(1, 3), at(t0, 500));
    // The same step read again: the clock runs on from step 3.
    p.read(&step(1, 3), at(t0, 3_000));
    assert_eq!(p.left(at(t0, 5_000)), Ok(Some(Duration::from_millis(500))));
    // A step after a slow one raises the pace.
    p.read(&step(1, 4), at(t0, 5_000));
    assert_eq!(p.left(at(t0, 5_000)), Ok(Some(Duration::from_secs(90))));
    // A second pass starts its steps again: still an advance.
    p.read(&step(2, 1), at(t0, 5_250));
    assert_eq!(p.left(at(t0, 5_250)), Ok(Some(Duration::from_secs(90))));
}

#[tokio::test(start_paused = true)]
async fn a_poll_that_does_not_answer_counts_as_the_stall() {
    let t0 = Instant::now();
    let mut p = Pace::new(t0);
    p.read(&step(1, 1), t0);
    p.read(&step(1, 2), t0 + Duration::from_millis(250));
    p.read(&step(1, 3), t0 + Duration::from_millis(500));
    tokio::time::advance(Duration::from_millis(500)).await;
    // No pace: the wait is not bounded.
    let fresh = Pace::new(Instant::now());
    assert_eq!(within(&fresh, async { 7 }).await, Ok(7));
    let never = within(&p, std::future::pending::<()>()).await;
    let stall = never.unwrap_err();
    assert_eq!(stall.slowest, Duration::from_millis(250));
    assert!(stall.quiet >= Duration::from_secs(5), "{stall}");
}
