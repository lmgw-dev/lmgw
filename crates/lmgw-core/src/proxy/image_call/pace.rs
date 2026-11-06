//! A followed job's pace (chat-media-tools design §5.4): whether it still
//! advances, judged from what its own polls have shown.
//!
//! A poll **advances** the job when it reads something the last one did not:
//! another status, another queue place, another `(pass, step)`. The job's
//! **pace** is the longest time it took between two advances while it was
//! generating — from the first `generating` read to its first step, and
//! from each step to the next — so the slow stages between steps (the text
//! encoder before the first, a hires pass's start) count, and the queue
//! wait behind another job does not. A job **stalls** once it has gone
//! [`STALL_PACES`] times its pace without advancing: the image server has
//! stopped working on it, or stopped answering for it (a poll still
//! unanswered then counts the same).
//!
//! Judged only once the job has stepped [`JUDGED_AFTER_STEPS`] times: before
//! that its pace says nothing yet (a first gap of one poll would make any
//! slow stage a stall).
//! A job that reports no steps — a row whose preview mode is off, a build
//! without previews — has no pace and is not judged: it is followed while
//! its image server answers for it, and a log line says so when a cancel
//! leaves it to be followed.

use std::time::Duration;

use tokio::time::Instant;

use serde_json::Value;

/// How many of its own slowest gaps a job may go without advancing before
/// it counts as stalled (module doc). Twenty leaves room for a stage after
/// the last step — the VAE decoding the picture — that takes longer than a
/// step; the bound it makes is named in every message that applies it.
pub(super) const STALL_PACES: u32 = 20;

/// How many steps a job must have taken before its pace is judged (module
/// doc): named with [`STALL_PACES`] in every stall warning.
pub(super) const JUDGED_AFTER_STEPS: u32 = 2;

/// What one poll read of the job's progress.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    status: String,
    queue_position: Option<u64>,
    /// `(pass, step)`, where the job reports them.
    step: Option<(u64, u64)>,
}

impl Mark {
    fn of(job: &Value) -> Self {
        let n = |v: Option<&Value>| v.and_then(Value::as_u64);
        let p = job.get("preview").filter(|p| p.is_object());
        Self {
            status: job
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            queue_position: n(job.get("queue_position")),
            step: p.and_then(|p| Some((n(p.get("pass"))?, n(p.get("step"))?))),
        }
    }
}

/// A job's pace, fed by every poll of it (module doc).
#[derive(Debug, Clone)]
pub(super) struct Pace {
    last: Option<Mark>,
    /// When `last` was first read: the job's last advance.
    since: Instant,
    /// The longest gap between two advances while generating.
    slowest: Duration,
    /// Step advances seen.
    steps: u32,
}

/// A job that stopped advancing, and the figures that say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stall {
    /// How long it went without advancing.
    pub quiet: Duration,
    /// Its slowest gap so far.
    pub slowest: Duration,
}

impl std::fmt::Display for Stall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "made no progress for {:.1} s, {STALL_PACES} times its slowest step so far ({:.1} s), \
             a pace judged once a job has taken {JUDGED_AFTER_STEPS} steps",
            self.quiet.as_secs_f64(),
            self.slowest.as_secs_f64()
        )
    }
}

impl Pace {
    /// A job submitted at `now`.
    pub fn new(now: Instant) -> Self {
        Self {
            last: None,
            since: now,
            slowest: Duration::ZERO,
            steps: 0,
        }
    }

    /// A poll read `job` at `now`.
    pub fn read(&mut self, job: &Value, now: Instant) {
        let mark = Mark::of(job);
        if self.last.as_ref() == Some(&mark) {
            return;
        }
        if let Some(last) = &self.last {
            if last.status == "generating" {
                self.slowest = self.slowest.max(now.duration_since(self.since));
            }
            if mark.step.is_some() && mark.step != last.step {
                self.steps += 1;
            }
        }
        self.last = Some(mark);
        self.since = now;
    }

    /// The longest the job may still go without advancing, from `now`:
    /// `Ok(None)` while it has no pace to judge by, `Err` once it stalled.
    pub fn left(&self, now: Instant) -> Result<Option<Duration>, Stall> {
        if self.steps < JUDGED_AFTER_STEPS {
            return Ok(None);
        }
        let bound = self.slowest * STALL_PACES;
        let quiet = now.duration_since(self.since);
        match bound.checked_sub(quiet) {
            Some(left) if !left.is_zero() => Ok(Some(left)),
            _ => Err(self.stall_at(now)),
        }
    }

    /// The job's stall as of `now`.
    fn stall_at(&self, now: Instant) -> Stall {
        Stall {
            quiet: now.duration_since(self.since),
            slowest: self.slowest,
        }
    }

    /// Whether the job has a pace to be judged by.
    pub fn judged(&self) -> bool {
        self.steps >= JUDGED_AFTER_STEPS
    }
}

/// `fut`, given up once the job stalls (`left`'s bound): `Err` then.
pub(super) async fn within<F: std::future::Future>(
    pace: &Pace,
    fut: F,
) -> Result<F::Output, Stall> {
    let Some(left) = pace.left(Instant::now())? else {
        return Ok(fut.await);
    };
    match tokio::time::timeout(left, fut).await {
        Ok(out) => Ok(out),
        Err(_) => Err(pace.stall_at(Instant::now())),
    }
}

#[cfg(test)]
mod tests;
