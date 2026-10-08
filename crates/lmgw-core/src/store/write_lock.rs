//! The waits for a database's write lock ([`begin_write`](super::begin_write)),
//! and what is said about them (the begin-write review's B-5).
//!
//! Each wait is said at debug. A wait past half the busy timeout is a
//! contention, and the first such wait while writers queue for one
//! database opens an episode with a warning; the episode lasts until no
//! writer waits for that database any more, which is when the queue behind
//! the slow holder has cleared. The waits past the threshold in between are
//! said at debug only, and the episode's end says how many there were, the
//! longest, and how many gave up. One slow holder used to warn once per
//! writer queued behind it: about 75 lines at 50 requests a second.
//!
//! A line names the waiter, never the holder: what held the lock is not
//! tracked, and it can be any write transaction or single write statement
//! on that database, on any connection.

use std::collections::HashMap;
use std::panic::Location;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use super::BUSY_TIMEOUT;

/// A wait longer than this is a contention: half the busy timeout, at
/// which a writer gives up with "database is locked".
pub(super) fn warn_past() -> Duration {
    BUSY_TIMEOUT / 2
}

/// How a wait for the write lock ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    GotIt,
    GaveUp,
    /// Its caller went away before it had the lock.
    Dropped,
}

impl Outcome {
    fn said(self) -> &'static str {
        match self {
            Self::GotIt => "got it",
            Self::GaveUp => "gave up",
            Self::Dropped => "was dropped by its caller",
        }
    }
}

/// A line to log, and its level.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Said {
    Debug(String),
    Info(String),
    Warn(String),
}

impl Said {
    fn log(self) {
        match self {
            Self::Debug(s) => tracing::debug!("{s}"),
            Self::Info(s) => tracing::info!("{s}"),
            Self::Warn(s) => tracing::warn!("{s}"),
        }
    }
}

/// The writers waiting for one database's write lock now, and the
/// contention episode they are in.
#[derive(Debug, Default)]
pub(super) struct Contention {
    waiting: usize,
    episode: Option<Episode>,
}

#[derive(Debug)]
struct Episode {
    /// The wait whose warning opened it.
    first_at: &'static Location<'static>,
    /// The waits past the threshold in it, that one included.
    long_waits: usize,
    gave_up: usize,
    longest: Duration,
    longest_at: &'static Location<'static>,
}

impl Contention {
    /// A writer starts waiting.
    pub(super) fn start(&mut self) {
        self.waiting += 1;
    }

    /// The wait of the writer at `at` on database `db` ended after `waited`:
    /// what to say about it.
    pub(super) fn end(
        &mut self,
        db: &str,
        waited: Duration,
        at: &'static Location<'static>,
        outcome: Outcome,
    ) -> Vec<Said> {
        self.waiting = self.waiting.saturating_sub(1);
        let ms = waited.as_millis();
        let mut said = vec![Said::Debug(format!(
            "store: a write transaction at {at} waited {ms} ms for a connection and the write \
             lock, and {}",
            outcome.said()
        ))];
        if waited > warn_past() {
            match &mut self.episode {
                Some(e) => {
                    e.long_waits += 1;
                    e.gave_up += usize::from(outcome == Outcome::GaveUp);
                    if waited > e.longest {
                        e.longest = waited;
                        e.longest_at = at;
                    }
                }
                None => {
                    said = vec![Said::Warn(format!(
                        "store: a write transaction at {at} waited {ms} ms for a connection and \
                         the write lock, and {} (warns past {} ms, half the busy timeout of {} \
                         ms). This names the waiter: what held the lock is not tracked, and can \
                         be any write transaction or single write statement on {db}. {} more \
                         writers wait for it now; the rest of this contention is said once none \
                         does.",
                        outcome.said(),
                        warn_past().as_millis(),
                        BUSY_TIMEOUT.as_millis(),
                        self.waiting
                    ))];
                    self.episode = Some(Episode {
                        first_at: at,
                        long_waits: 1,
                        gave_up: usize::from(outcome == Outcome::GaveUp),
                        longest: waited,
                        longest_at: at,
                    });
                }
            }
        }
        if self.waiting == 0 {
            said.extend(self.episode.take().and_then(|e| e.ended(db)));
        }
        said
    }

    /// Nothing waits and no episode is open: the entry can go.
    fn idle(&self) -> bool {
        self.waiting == 0 && self.episode.is_none()
    }
}

impl Episode {
    /// What the end of an episode says: nothing for one wait alone, which
    /// its warning said all of.
    fn ended(self, db: &str) -> Option<Said> {
        if self.long_waits == 1 {
            return None;
        }
        let text = format!(
            "store: the contention for the write lock on {db} that the wait at {} warned about \
             has cleared: {} write transactions waited past {} ms, the longest {} ms at {}, and \
             {} of them gave up",
            self.first_at,
            self.long_waits,
            warn_past().as_millis(),
            self.longest.as_millis(),
            self.longest_at,
            self.gave_up
        );
        // A writer that gave up lost its write: that is worth a warning of
        // its own.
        Some(if self.gave_up > 0 {
            Said::Warn(text)
        } else {
            Said::Info(text)
        })
    }
}

/// Every database's [`Contention`], by its file name; an entry goes once
/// nothing waits for that database.
static CONTENTION: LazyLock<Mutex<HashMap<PathBuf, Contention>>> = LazyLock::new(Default::default);

/// One writer waiting for a database's write lock: counted from
/// [`start`](Self::start) to [`end`](Self::end), or to its drop when its
/// caller went away mid-wait.
pub(super) struct Waiting {
    db: PathBuf,
    at: &'static Location<'static>,
    started: Instant,
    ended: bool,
}

impl Waiting {
    pub(super) fn start(db: PathBuf, at: &'static Location<'static>) -> Self {
        CONTENTION
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(db.clone())
            .or_default()
            .start();
        Self {
            db,
            at,
            started: Instant::now(),
            ended: false,
        }
    }

    pub(super) fn end(mut self, outcome: Outcome) {
        self.finish(outcome);
    }

    fn finish(&mut self, outcome: Outcome) {
        self.ended = true;
        let waited = self.started.elapsed();
        let said = {
            let mut all = CONTENTION.lock().unwrap_or_else(|e| e.into_inner());
            let Some(c) = all.get_mut(&self.db) else {
                return;
            };
            let said = c.end(&self.db.display().to_string(), waited, self.at, outcome);
            if c.idle() {
                all.remove(&self.db);
            }
            said
        };
        // Logged outside the lock.
        for s in said {
            s.log();
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if !self.ended {
            self.finish(Outcome::Dropped);
        }
    }
}

#[cfg(test)]
impl Waiting {
    /// This wait, as one that started `ago` before now: for a test of a wait
    /// past the threshold that does not wait that long.
    pub(super) fn started_ago(mut self, ago: Duration) -> Self {
        self.started = Instant::now() - ago;
        self
    }
}

/// The writers waiting for `db`'s write lock now, and whether a contention
/// episode is open on it; `None` once its entry went, which is when neither
/// is the case.
#[cfg(test)]
pub(super) fn contention_of(db: &std::path::Path) -> Option<(usize, bool)> {
    let all = CONTENTION.lock().unwrap_or_else(|e| e.into_inner());
    all.get(db).map(|c| (c.waiting, c.episode.is_some()))
}
