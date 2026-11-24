//! A file's own clock: docling's `document_timeout`, counted as conversion
//! time. It starts when the file's first step gets a slot of the tool pool
//! and stops while its steps wait: for a slot ([`super::ToolPool`]'s
//! acquires pause it) or for the layout model's gate
//! (`layout_model::session`). So a file that queues behind other files is
//! not late; what it spends working counts.
//!
//! The clock is task-local ([`scope`]); a step outside one (a request
//! without `document_timeout`, Chat's reads) finds none and nothing is
//! counted. [`paused`] nests (a wait for slots inside the wait for the
//! gate).
//!
//! **Lanes.** Most of a file's steps run one after the other. Steps that
//! run beside each other (a PDF's picture runs beside its text) each run in
//! a lane of their own ([`beside`]), and the clock counts while any lane of
//! the file works: one step waiting for a slot does not stop the count of
//! another that works, and the file counts nothing only while every lane
//! waits. The lane that starts lanes beside it waits for them
//! ([`paused`]) while they run.
//!
//! **Who ends the file at its deadline.** The batch does
//! (`batch::within_deadline`: `deadline_exceeded`, the file's tools
//! stopped), until a stage that ends its own work at the deadline takes the
//! end over ([`FileClock::hand_over`]): the page-reading model's stage,
//! which keeps what was read. From then on the batch lets the file finish,
//! and the steps still working at the deadline end themselves
//! ([`FileClock::expiry`], [`FileClock::handed_expiry`]).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Debug, Default)]
struct State {
    started: bool,
    /// Counted before the run now going on.
    used: Duration,
    /// Counting since, while started and a lane works.
    since: Option<Instant>,
    /// The file's lanes (module doc): the first, and those beside it.
    lanes: u32,
    /// A stage took the end over (module doc).
    handed: bool,
    /// The lanes that wait.
    waiting: u32,
}

impl State {
    /// Count from now, or stop counting, as the lanes say.
    fn settle(&mut self) {
        let works = self.started && self.waiting < self.lanes;
        match (works, self.since) {
            (true, None) => self.since = Some(Instant::now()),
            (false, Some(since)) => {
                self.used += since.elapsed();
                self.since = None;
            }
            _ => {}
        }
    }
}

/// One file's clock against `limit`.
#[derive(Debug)]
pub struct FileClock {
    limit: Duration,
    state: Mutex<State>,
    /// Told of every start, pause and resume.
    changed: watch::Sender<()>,
}

impl FileClock {
    pub fn new(limit: Duration) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: Mutex::new(State {
                lanes: 1,
                ..State::default()
            }),
            changed: watch::Sender::new(()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn limit(&self) -> Duration {
        self.limit
    }

    /// Change the state under its lock, settle it, and tell the watchers.
    fn change(&self, f: impl FnOnce(&mut State)) {
        let mut s = self.lock();
        f(&mut s);
        s.settle();
        drop(s);
        self.changed.send_replace(());
    }

    /// The first slot was granted: the clock runs from now (once a lane
    /// works).
    fn start(&self) {
        if self.lock().started {
            return;
        }
        self.change(|s| s.started = true);
    }

    /// `lane` waits (nested: only its first wait counts).
    fn pause(&self, lane: &AtomicU32) {
        self.change(|s| {
            if lane.fetch_add(1, Ordering::SeqCst) == 0 {
                s.waiting = s.waiting.saturating_add(1);
            }
        });
    }

    fn resume(&self, lane: &AtomicU32) {
        self.change(|s| {
            // Under the state's lock, as every change of a lane is.
            let was = lane.load(Ordering::SeqCst);
            lane.store(was.saturating_sub(1), Ordering::SeqCst);
            if was == 1 {
                s.waiting = s.waiting.saturating_sub(1);
            }
        });
    }

    fn open_lane(&self) {
        self.change(|s| s.lanes = s.lanes.saturating_add(1));
    }

    /// A lane beside the first ended, waiting or not.
    fn close_lane(&self, lane: &AtomicU32) {
        self.change(|s| {
            if lane.swap(0, Ordering::SeqCst) > 0 {
                s.waiting = s.waiting.saturating_sub(1);
            }
            s.lanes = s.lanes.saturating_sub(1);
        });
    }

    /// The conversion time counted so far.
    pub fn used(&self) -> Duration {
        let s = self.lock();
        s.used + s.since.map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// While the clock runs, the time left before `limit`; `None` while it
    /// has not started or no lane works.
    pub fn left_while_running(&self) -> Option<Duration> {
        let s = self.lock();
        let since = s.since?;
        Some(self.limit.saturating_sub(s.used + since.elapsed()))
    }

    pub fn expired(&self) -> bool {
        self.used() >= self.limit
    }

    /// Told of every change ([`watch::Receiver::changed`]).
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Done once the clock has used its limit: never while it stands still
    /// (no lane works, or it has not started).
    pub async fn expiry(&self) {
        let mut changed = self.subscribe();
        loop {
            changed.borrow_and_update();
            if self.expired() {
                return;
            }
            // While the clock stands still, only a change can make it late.
            match self.left_while_running() {
                Some(d) => {
                    tokio::select! {
                        () = tokio::time::sleep(d) => {}
                        _ = changed.changed() => {}
                    }
                }
                // The sender lives in `self`: a change always comes.
                None => {
                    let _ = changed.changed().await;
                }
            }
        }
    }

    /// A stage takes over ending the file at its deadline (module doc).
    pub fn hand_over(&self) {
        self.change(|s| s.handed = true);
    }

    /// Whether a stage took the end over.
    pub fn handed_over(&self) -> bool {
        self.lock().handed
    }

    /// Done once a stage took the end over and the clock has used its
    /// limit: what a step beside that stage (a picture run) ends at.
    pub async fn handed_expiry(&self) {
        let mut changed = self.subscribe();
        loop {
            changed.borrow_and_update();
            if self.handed_over() {
                return self.expiry().await;
            }
            let _ = changed.changed().await;
        }
    }
}

/// The file's clock and the lane a step runs in (its waits, nested).
#[derive(Clone)]
struct Here {
    clock: Arc<FileClock>,
    lane: Arc<AtomicU32>,
}

tokio::task_local! {
    static CLOCK: Here;
}

/// Run `fut` with `clock` as its file's clock, in the file's first lane.
pub async fn scope<F: std::future::Future>(clock: Arc<FileClock>, fut: F) -> F::Output {
    let here = Here {
        clock,
        lane: Arc::new(AtomicU32::new(0)),
    };
    CLOCK.scope(here, fut).await
}

/// The clock of the file this step belongs to, when it has one.
pub fn current() -> Option<Arc<FileClock>> {
    CLOCK.try_with(|h| Arc::clone(&h.clock)).ok()
}

/// Closes a lane when its step ends or is dropped.
struct Lane(Here);

impl Drop for Lane {
    fn drop(&mut self) {
        self.0.clock.close_lane(&self.0.lane);
    }
}

/// Run `fut` in a lane of its own beside the file's others (module doc);
/// the caller waits for it inside [`paused`]. Without a clock, `fut` as it
/// is.
pub async fn beside<F: std::future::Future>(fut: F) -> F::Output {
    let Ok(clock) = CLOCK.try_with(|h| Arc::clone(&h.clock)) else {
        return fut.await;
    };
    clock.open_lane();
    let here = Here {
        clock,
        lane: Arc::new(AtomicU32::new(0)),
    };
    let _lane = Lane(here.clone());
    CLOCK.scope(here, fut).await
}

/// A slot was granted to one of the file's steps.
pub(super) fn slot_granted() {
    if let Some(c) = current() {
        c.start();
    }
}

/// A step that takes no slot started working (a speech-to-text call, which
/// is a model call and takes none, D20): the file's clock runs from now, as
/// a granted slot starts it.
pub fn started() {
    slot_granted();
}

/// The step's lane waits until this is dropped.
#[must_use = "the lane works again when this is dropped"]
pub struct Paused(Option<Here>);

impl Drop for Paused {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.clock.resume(&h.lane);
        }
    }
}

/// The step's lane waits: the file's clock stops unless another lane
/// works (nothing without a clock).
pub fn paused() -> Paused {
    let h = CLOCK.try_with(Here::clone).ok();
    if let Some(h) = &h {
        h.clock.pause(&h.lane);
    }
    Paused(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_clock_counts_work_and_not_waits() {
        let c = FileClock::new(Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(5)).await;
        // Not started: nothing counts.
        assert_eq!(c.used(), Duration::ZERO);
        assert_eq!(c.left_while_running(), None);
        let (cc, c2) = (c.clone(), c.clone());
        scope(cc, async move {
            {
                let _p = paused();
                slot_granted();
                tokio::time::advance(Duration::from_secs(3)).await;
            }
            assert_eq!(c2.used(), Duration::ZERO);
            tokio::time::advance(Duration::from_secs(4)).await;
            {
                let _outer = paused();
                let _inner = paused();
                tokio::time::advance(Duration::from_secs(30)).await;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
        })
        .await;
        assert_eq!(c.used(), Duration::from_secs(5));
        assert_eq!(c.left_while_running(), Some(Duration::from_secs(5)));
        assert!(!c.expired());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(c.expired());
        // Outside a scope there is no clock to pause.
        assert!(current().is_none());
        drop(paused());
    }

    #[tokio::test(start_paused = true)]
    async fn lanes_beside_each_other_count_while_any_works() {
        let c = FileClock::new(Duration::from_secs(100));
        let c2 = c.clone();
        scope(c.clone(), async move {
            slot_granted();
            let _join = paused();
            // Lane a works 4 s while lane b waits for a slot; then a waits
            // 2 s while b works, b works 1 s more after a ended, and then
            // every lane waits 10 s.
            let a = beside(async {
                tokio::time::sleep(Duration::from_secs(4)).await;
                let _w = paused();
                tokio::time::sleep(Duration::from_secs(2)).await;
            });
            let b = beside(async {
                {
                    let _w = paused();
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
                let _w = paused();
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
            futures::future::join(a, b).await;
            // 4 + 2 + 1 s; the 10 s every lane waited are not counted.
            assert_eq!(c2.used(), Duration::from_secs(7));
        })
        .await;
        // Every lane closed: the first works again (the join's wait is over).
        assert!(c.left_while_running().is_some());
        // A dropped lane that was waiting gives its wait back.
        let c3 = c.clone();
        scope(c.clone(), async move {
            let f = beside(async {
                let _w = paused();
                std::future::pending::<()>().await;
            });
            let _ = tokio::time::timeout(Duration::from_secs(1), f).await;
            let before = c3.used();
            tokio::time::advance(Duration::from_secs(2)).await;
            assert_eq!(c3.used(), before + Duration::from_secs(2));
        })
        .await;
    }

    /// The expiry counts work only, and a step beside a stage that took
    /// the file's end over ends at it, never before the hand-over.
    #[tokio::test(start_paused = true)]
    async fn expiry_counts_work_and_a_handed_over_end_reaches_the_steps_beside() {
        let c = FileClock::new(Duration::from_secs(10));
        let c2 = c.clone();
        let beside_cut = scope(c.clone(), async move {
            slot_granted();
            let started = Instant::now();
            // 4 s of work, 30 s of waiting, then work until the limit.
            let expiry = c2.expiry();
            tokio::pin!(expiry);
            let work = async {
                tokio::time::sleep(Duration::from_secs(4)).await;
                {
                    let _w = paused();
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                std::future::pending::<()>().await;
            };
            tokio::select! {
                () = &mut expiry => {}
                () = work => unreachable!(),
            }
            assert_eq!(started.elapsed(), Duration::from_secs(40));
            assert!(c2.expired());
            // Expired, but nothing took the end over: a step beside a
            // stage waits on.
            assert!(!c2.handed_over());
            let beside = c2.handed_expiry();
            tokio::pin!(beside);
            assert!(tokio::time::timeout(Duration::from_secs(1), &mut beside)
                .await
                .is_err());
            c2.hand_over();
            tokio::time::timeout(Duration::from_secs(1), beside)
                .await
                .is_ok()
        })
        .await;
        assert!(beside_cut);
        assert!(c.handed_over());
    }
}
