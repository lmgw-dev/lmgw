//! One sync at a time, and its progress for every browser that asks.
//!
//! The hub keeps **the current (or last) run's events in a buffer** and fans
//! each new one out over a broadcast channel. A subscriber gets the run's
//! header ([`RunInfo`]), then every buffered event, then the live ones — the
//! snapshot and the subscription are taken under one lock, and every event is
//! appended and sent under the same lock, so nothing falls between the two
//! and nothing arrives twice. A browser that opens the page mid-sync therefore
//! sees the whole run so far, not just what happens after it connected.
//!
//! **The buffer has no cap.** A run's events are bounded by the folder
//! itself: one `scanning` and one `planned`, one terminal event (`done` or
//! `aborted`), an `index_reset` at most, and per file one outcome (`file_done`,
//! `unchanged`, `skipped`, `error`, `removed`) plus one `embedding` event per
//! batch of [`crate::sync::EMBED_BATCH_SIZE`] chunks. The buffer is cleared
//! when the next run starts.
//!
//! **A lagged subscriber is never silently skipped ahead.** The broadcast
//! channel holds [`LIVE_FANOUT_CAPACITY`] events for a slow reader; one that
//! falls further behind than that re-reads the buffer from the last event it
//! delivered, so it still sees every event, in order. The capacity is a
//! latency trade, not a limit on what a reader receives.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::sync::SyncEvent;

/// How many events the live fan-out holds for a subscriber that has not read
/// them yet. A subscriber further behind re-reads the buffer (see the module
/// docs), so this bounds memory per slow reader, never what it sees.
pub const LIVE_FANOUT_CAPACITY: usize = 256;

/// One run's header: which run, whether it is still going, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunInfo {
    /// `0` until the first run starts; then 1, 2, … for this process.
    pub run: u64,
    pub running: bool,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
}

/// What a subscriber receives, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum HubItem {
    /// A run began or ended — or, first on every subscription, where things
    /// stand. The events that follow belong to this run.
    Run(RunInfo),
    Event(Arc<SyncEvent>),
}

#[derive(Debug, Clone)]
enum Msg {
    Run(RunInfo),
    Event {
        run: u64,
        seq: usize,
        event: Arc<SyncEvent>,
    },
}

struct State {
    info: RunInfo,
    events: Vec<Arc<SyncEvent>>,
}

pub struct SyncHub {
    state: Mutex<State>,
    tx: broadcast::Sender<Msg>,
}

impl Default for SyncHub {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SyncHub {
    pub fn new() -> Self {
        Self::with_capacity(LIVE_FANOUT_CAPACITY)
    }

    /// With a given fan-out capacity — the lag test uses a tiny one.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self {
            state: Mutex::new(State {
                info: RunInfo {
                    run: 0,
                    running: false,
                    started_ms: None,
                    finished_ms: None,
                },
                events: Vec::new(),
            }),
            tx,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding this lock leaves plain data behind; keep going.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn info(&self) -> RunInfo {
        self.lock().info.clone()
    }

    pub fn running(&self) -> bool {
        self.lock().info.running
    }

    /// Claim the next run, clearing the last one's events. `None` while a run
    /// is going — the caller answers `409 sync_running`.
    pub fn try_begin(&self) -> Option<u64> {
        let mut st = self.lock();
        if st.info.running {
            return None;
        }
        st.info = RunInfo {
            run: st.info.run + 1,
            running: true,
            started_ms: Some(now_ms()),
            finished_ms: None,
        };
        st.events.clear();
        let _ = self.tx.send(Msg::Run(st.info.clone()));
        Some(st.info.run)
    }

    /// Append one event of `run` and send it to every subscriber.
    pub fn push(&self, run: u64, event: SyncEvent) {
        let mut st = self.lock();
        if st.info.run != run {
            return;
        }
        let event = Arc::new(event);
        st.events.push(event.clone());
        let seq = st.events.len() - 1;
        let _ = self.tx.send(Msg::Event { run, seq, event });
    }

    /// Mark `run` over. Its events stay buffered for the next subscriber.
    pub fn finish(&self, run: u64) {
        let mut st = self.lock();
        if st.info.run != run {
            return;
        }
        st.info.running = false;
        st.info.finished_ms = Some(now_ms());
        let _ = self.tx.send(Msg::Run(st.info.clone()));
    }

    /// The run's header and events so far, and a receiver for what follows —
    /// taken together, under the lock.
    fn snapshot(&self) -> (RunInfo, Vec<Arc<SyncEvent>>, broadcast::Receiver<Msg>) {
        let st = self.lock();
        (st.info.clone(), st.events.clone(), self.tx.subscribe())
    }

    /// Everything of the current (or last) run so far, then live — see the
    /// module docs. The stream never ends by itself; the server ends it at
    /// shutdown.
    pub fn subscribe(self: &Arc<Self>) -> BoxStream<'static, HubItem> {
        let (info, events, rx) = self.snapshot();
        let mut pending: VecDeque<HubItem> = VecDeque::with_capacity(events.len() + 1);
        let sub = Sub {
            hub: self.clone(),
            rx,
            run: info.run,
            next_seq: events.len(),
            pending: {
                pending.push_back(HubItem::Run(info));
                pending.extend(events.into_iter().map(HubItem::Event));
                pending
            },
        };
        futures::stream::unfold(sub, |mut sub| async move {
            loop {
                if let Some(item) = sub.pending.pop_front() {
                    return Some((item, sub));
                }
                match sub.rx.recv().await {
                    Ok(Msg::Run(info)) => {
                        if info.run != sub.run {
                            sub.run = info.run;
                            sub.next_seq = 0;
                        }
                        sub.pending.push_back(HubItem::Run(info));
                    }
                    Ok(Msg::Event { run, seq, event }) => {
                        if run == sub.run && seq == sub.next_seq {
                            sub.next_seq += 1;
                            sub.pending.push_back(HubItem::Event(event));
                        } else if run == sub.run && seq < sub.next_seq {
                            // Already delivered from the buffer.
                        } else {
                            // A gap: resynchronise from the buffer.
                            sub.catch_up();
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => sub.catch_up(),
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }
}

struct Sub {
    hub: Arc<SyncHub>,
    rx: broadcast::Receiver<Msg>,
    run: u64,
    next_seq: usize,
    pending: VecDeque<HubItem>,
}

impl Sub {
    /// Fell behind the live channel: take a fresh receiver and re-read the
    /// buffer from the first event not yet delivered (the whole run, headed
    /// by its [`RunInfo`], if a new one started meanwhile).
    fn catch_up(&mut self) {
        let (info, events, rx) = self.hub.snapshot();
        self.rx = rx;
        if info.run != self.run {
            self.run = info.run;
            self.next_seq = 0;
            self.pending.push_back(HubItem::Run(info));
        }
        let from = self.next_seq.min(events.len());
        self.pending
            .extend(events[from..].iter().cloned().map(HubItem::Event));
        self.next_seq = events.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(i: usize) -> SyncEvent {
        SyncEvent::FileDone {
            file: format!("f{i}.md"),
            chunks: 1,
        }
    }

    fn files(items: &[HubItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                HubItem::Event(e) => match e.as_ref() {
                    SyncEvent::FileDone { file, .. } => Some(file.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    async fn take(s: &mut BoxStream<'static, HubItem>, n: usize) -> Vec<HubItem> {
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(
                tokio::time::timeout(std::time::Duration::from_secs(5), s.next())
                    .await
                    .expect("the stream stalled")
                    .expect("the stream ended"),
            );
        }
        out
    }

    #[tokio::test]
    async fn a_late_subscriber_gets_the_run_so_far_then_live_events() {
        let hub = Arc::new(SyncHub::new());
        let run = hub.try_begin().unwrap();
        assert!(hub.try_begin().is_none(), "one run at a time");
        hub.push(run, ev(0));
        hub.push(run, ev(1));
        let mut s = hub.subscribe();
        hub.push(run, ev(2));
        let got = take(&mut s, 4).await;
        assert!(matches!(&got[0], HubItem::Run(i) if i.run == run && i.running));
        assert_eq!(files(&got), ["f0.md", "f1.md", "f2.md"]);
        hub.finish(run);
        let got = take(&mut s, 1).await;
        assert!(matches!(&got[0], HubItem::Run(i) if !i.running));
    }

    #[tokio::test]
    async fn a_lagged_subscriber_rereads_the_buffer_instead_of_skipping() {
        let hub = Arc::new(SyncHub::with_capacity(2));
        let run = hub.try_begin().unwrap();
        let mut s = hub.subscribe();
        for i in 0..20 {
            hub.push(run, ev(i));
        }
        let got = take(&mut s, 21).await;
        let want: Vec<String> = (0..20).map(|i| format!("f{i}.md")).collect();
        assert_eq!(files(&got), want, "every event, in order, once");
    }

    #[tokio::test]
    async fn a_new_run_clears_the_buffer_and_says_so() {
        let hub = Arc::new(SyncHub::new());
        let r1 = hub.try_begin().unwrap();
        hub.push(r1, ev(0));
        hub.finish(r1);
        let r2 = hub.try_begin().unwrap();
        hub.push(r2, ev(9));
        let mut s = hub.subscribe();
        let got = take(&mut s, 2).await;
        assert!(matches!(&got[0], HubItem::Run(i) if i.run == r2));
        assert_eq!(files(&got), ["f9.md"]);
        // A stale run's event is dropped, not mixed in.
        hub.push(r1, ev(1));
        hub.push(r2, ev(10));
        assert_eq!(files(&take(&mut s, 1).await), ["f10.md"]);
    }
}
