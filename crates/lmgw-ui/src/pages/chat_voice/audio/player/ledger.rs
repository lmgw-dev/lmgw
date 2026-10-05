//! The player's bookkeeping (chat-voice §11.1), apart from the browser so it
//! is tested natively: which items take audio, what the worklet reported for
//! each, the final count of an item that played out or was flushed, and who
//! waits for which answer. [`super::Player`] feeds it the worklet's messages
//! and posts what it decides.
//!
//! The rules it keeps (the barge-in traps of the WP6 review, M4):
//! - **A closed item stays closed.** An item takes pushes from [`Ledger::begin`]
//!   until [`Ledger::end`] or a flush closes it. A push after that — the
//!   realtime deltas still in flight when a barge-in flushed the response —
//!   is dropped and counted ([`Ledger::late`]), never queued again.
//! - **A count outlives its item.** The final count of an item that played
//!   out or was flushed is kept until the owner releases it, or until a later
//!   item closes; [`Ledger::played`] answers it.
//! - **A flush after play-out still answers.** Flushing an item that has just
//!   played to its end answers its final count, and an `end` waiter gets the
//!   count when the worklet says it played out — a flush only turns a waiter's
//!   answer into `None` when the worklet really dropped audio of that item.
//! - **Heard, not rendered.** `played` counts samples rendered into the graph;
//!   `heard` takes off the output latency the browser reports, as truncate's
//!   `audio_end_ms` needs (see [`heard`]).

use std::collections::{HashMap, HashSet};

use futures::channel::oneshot;

use super::super::pcm::{Pcm16Reader, RATE};

/// One item's counts, in samples at 24 kHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ItemCount {
    pub item: u32,
    pub pushed: u64,
    /// Rendered into the graph by the worklet.
    pub played: u64,
    /// `played` less what was still between the graph and the speaker when
    /// it was counted (the output latency the browser reports): truncate's
    /// `audio_end_ms` (§8.4, WP9).
    pub heard: u64,
}

/// How an item closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closed {
    /// It played to its end.
    Ended,
    /// A flush dropped what was left of it.
    Flushed,
}

#[derive(Debug, Clone, Copy)]
struct Final {
    count: ItemCount,
    how: Closed,
    /// When it closed (the page's clock, ms).
    at_ms: f64,
    /// The worklet message that closed it: finals of one message are kept
    /// together.
    event: u64,
}

/// What became of a push.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Push {
    /// The whole samples to post (empty: an odd byte waits for the next).
    Post(Vec<u8>),
    /// The item is closed: dropped. `first` is the first drop for it, the
    /// one worth a log line.
    Late { first: bool },
}

/// What a flush asked.
struct FlushReq {
    scope: Option<u32>,
    reply: oneshot::Sender<Vec<ItemCount>>,
}

pub(crate) type EndRx = oneshot::Receiver<Option<ItemCount>>;
pub(crate) type FlushRx = oneshot::Receiver<Vec<ItemCount>>;

/// See the module docs.
#[derive(Default)]
pub(crate) struct Ledger {
    next_item: u32,
    next_flush: u32,
    /// Items that take pushes, each with its odd-byte carry.
    open: HashMap<u32, Pcm16Reader>,
    /// Items still in the worklet: open, or ended and still playing.
    live: HashSet<u32>,
    /// The worklet's latest count of each live item.
    counts: HashMap<u32, ItemCount>,
    finals: HashMap<u32, Final>,
    ending: HashMap<u32, Vec<oneshot::Sender<Option<ItemCount>>>>,
    flushes: HashMap<u32, FlushReq>,
    /// Worklet messages that closed items, numbered.
    events: u64,
    late_pushes: u64,
    late_bytes: u64,
    late_logged: Option<u32>,
    underruns: u64,
}

/// `played` less the part of the output latency that had not played out yet
/// `elapsed_ms` after the count (an item counted as it is flushed: 0).
pub(crate) fn heard(played: u64, latency: u64, elapsed_ms: f64) -> u64 {
    let elapsed = (elapsed_ms.max(0.0) * f64::from(RATE) / 1000.0) as u64;
    played.saturating_sub(latency.saturating_sub(elapsed))
}

impl Ledger {
    /// A new item that takes pushes. One playback per page (§6.5, §11.1):
    /// the caller flushes every other live item first, see [`Self::others`].
    pub(crate) fn begin(&mut self) -> u32 {
        // 2^32 items in one page's life cannot happen; ids stay plain and
        // monotonic, so a late push can never meet a reused id.
        self.next_item += 1;
        let item = self.next_item;
        self.open.insert(item, Pcm16Reader::default());
        self.live.insert(item);
        item
    }

    /// Are items still in the worklet (so a flush has something to drop)?
    pub(crate) fn any_live(&self) -> bool {
        !self.live.is_empty()
    }

    /// Queue bytes for `item`, or drop them if it is closed.
    pub(crate) fn push(&mut self, item: u32, bytes: &[u8]) -> Push {
        match self.open.get_mut(&item) {
            Some(r) => Push::Post(r.feed(bytes)),
            None => {
                self.late_pushes += 1;
                self.late_bytes += bytes.len() as u64;
                let first = self.late_logged != Some(item);
                self.late_logged = Some(item);
                Push::Late { first }
            }
        }
    }

    /// Pushes dropped because their item was closed, and their bytes (the
    /// tests' view; the page logs the first late push of an item).
    #[cfg(test)]
    pub(crate) fn late(&self) -> (u64, u64) {
        (self.late_pushes, self.late_bytes)
    }

    /// `item` is complete. The receiver answers its count once it played
    /// out, `None` if a flush dropped audio of it first. `true`: post `end`
    /// to the worklet (the item is still in it).
    pub(crate) fn end(&mut self, item: u32, latency: u64, now_ms: f64) -> (EndRx, bool) {
        let (tx, rx) = oneshot::channel();
        if let Some(f) = self.finals.get(&item) {
            let _ = tx.send(match f.how {
                Closed::Ended => Some(self.answer(f, latency, now_ms)),
                Closed::Flushed => None,
            });
            return (rx, false);
        }
        if !self.live.contains(&item) {
            let _ = tx.send(None);
            return (rx, false);
        }
        // An odd byte left over is half a sample: dropped.
        let post = self.open.remove(&item).is_some();
        self.ending.entry(item).or_default().push(tx);
        (rx, post)
    }

    /// Start a flush of `scope` (one item, or every item): its items take
    /// no more pushes from now on. Returns the flush id, the receiver of the
    /// answer, and whether to post it: with nothing of the scope left in the
    /// worklet the answer (the final counts) is there at once.
    pub(crate) fn flush(
        &mut self,
        scope: Option<u32>,
        latency: u64,
        now_ms: f64,
    ) -> (u32, FlushRx, bool) {
        self.next_flush = self.next_flush.wrapping_add(1);
        let id = self.next_flush;
        match scope {
            Some(item) => {
                self.open.remove(&item);
            }
            None => self.open.clear(),
        }
        let (reply, rx) = oneshot::channel();
        let in_worklet = match scope {
            Some(item) => self.live.contains(&item),
            None => !self.live.is_empty(),
        };
        if !in_worklet {
            let mut done: Vec<ItemCount> = self
                .finals
                .iter()
                .filter(|(item, _)| scope.is_none_or(|s| s == **item))
                .map(|(_, f)| self.answer(f, latency, now_ms))
                .collect();
            done.sort_by_key(|c| c.item);
            let _ = reply.send(done);
            return (id, rx, false);
        }
        self.flushes.insert(id, FlushReq { scope, reply });
        (id, rx, true)
    }

    /// The worklet's progress report for `item`.
    pub(crate) fn on_progress(&mut self, item: u32, played: u64) {
        if self.live.contains(&item) {
            let c = self.counts.entry(item).or_insert(ItemCount {
                item,
                ..Default::default()
            });
            c.played = played;
        }
    }

    /// The queue ran dry while an item still waited for audio.
    pub(crate) fn on_underrun(&mut self) {
        self.underruns += 1;
    }

    #[cfg(test)]
    pub(crate) fn underruns(&self) -> u64 {
        self.underruns
    }

    /// `ended`: the item played to its end.
    pub(crate) fn on_ended(&mut self, c: ItemCount, latency: u64, now_ms: f64) {
        self.events += 1;
        let f = Final {
            count: c,
            how: Closed::Ended,
            at_ms: now_ms,
            event: self.events,
        };
        self.close(f);
        let answer = self.answer(&f, latency, now_ms);
        for w in self.ending.remove(&c.item).unwrap_or_default() {
            let _ = w.send(Some(answer));
        }
        self.prune(c.item);
    }

    /// `flushed`: the worklet dropped what it held of `items`; answer the
    /// flush `id` asked for.
    pub(crate) fn on_flushed(&mut self, id: u32, items: &[ItemCount], latency: u64, now_ms: f64) {
        self.events += 1;
        let event = self.events;
        let mut answered = Vec::new();
        for c in items {
            let count = ItemCount {
                heard: heard(c.played, latency, 0.0),
                ..*c
            };
            self.close(Final {
                count,
                how: Closed::Flushed,
                at_ms: now_ms,
                event,
            });
            for w in self.ending.remove(&c.item).unwrap_or_default() {
                let _ = w.send(None);
            }
            answered.push(count);
        }
        if let Some(req) = self.flushes.remove(&id) {
            // Items of the scope that had played out before the flush got
            // there: their final counts.
            let mut extra: Vec<ItemCount> = self
                .finals
                .iter()
                .filter(|(item, f)| f.event != event && req.scope.is_none_or(|s| s == **item))
                .map(|(_, f)| self.answer(f, latency, now_ms))
                .collect();
            extra.sort_by_key(|c| c.item);
            let _ = req
                .reply
                .send(answered.iter().copied().chain(extra).collect());
        }
        if let Some(newest) = items.iter().map(|c| c.item).max() {
            self.prune(newest);
        }
    }

    /// A flush the worklet did not answer in time (a suspended context may
    /// not run it): the last counts known for its scope. The worklet's
    /// answer, when it comes, still closes the items.
    pub(crate) fn flush_timed_out(&mut self, id: u32, latency: u64, now_ms: f64) -> Vec<ItemCount> {
        let Some(req) = self.flushes.remove(&id) else {
            return Vec::new();
        };
        let mut out: Vec<ItemCount> = self
            .live
            .iter()
            .filter(|i| req.scope.is_none_or(|s| s == **i))
            .map(|i| {
                let c = self.counts.get(i).copied().unwrap_or(ItemCount {
                    item: *i,
                    ..Default::default()
                });
                ItemCount {
                    heard: heard(c.played, latency, 0.0),
                    ..c
                }
            })
            .chain(
                self.finals
                    .iter()
                    .filter(|(i, _)| req.scope.is_none_or(|s| s == **i))
                    .map(|(_, f)| self.answer(f, latency, now_ms)),
            )
            .collect();
        out.sort_by_key(|c| c.item);
        out
    }

    /// Samples of `item` rendered so far (its final count once closed);
    /// `None` for an item unknown or released.
    pub(crate) fn played(&self, item: u32) -> Option<u64> {
        if let Some(f) = self.finals.get(&item) {
            return Some(f.count.played);
        }
        self.live
            .contains(&item)
            .then(|| self.counts.get(&item).map_or(0, |c| c.played))
    }

    /// Samples of `item` heard so far: [`Self::played`] less what the output
    /// latency still held.
    pub(crate) fn heard(&self, item: u32, latency: u64, now_ms: f64) -> Option<u64> {
        if let Some(f) = self.finals.get(&item) {
            return Some(self.answer(f, latency, now_ms).heard);
        }
        self.played(item).map(|p| heard(p, latency, 0.0))
    }

    /// The owner is done with `item`'s final count.
    pub(crate) fn release(&mut self, item: u32) {
        self.finals.remove(&item);
    }

    fn close(&mut self, f: Final) {
        let item = f.count.item;
        self.open.remove(&item);
        self.live.remove(&item);
        self.counts.remove(&item);
        self.finals.insert(item, f);
    }

    /// An item's final count as answered now: one that played out had all
    /// of it heard once its tail left the output.
    fn answer(&self, f: &Final, latency: u64, now_ms: f64) -> ItemCount {
        match f.how {
            Closed::Flushed => f.count,
            Closed::Ended => ItemCount {
                heard: heard(f.count.played, latency, now_ms - f.at_ms),
                ..f.count
            },
        }
    }

    /// Keep finals of the message that just closed `newest` and of later
    /// items; older ones were superseded (released or not).
    fn prune(&mut self, newest: u32) {
        let event = self.events;
        self.finals
            .retain(|item, f| f.event == event || *item > newest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(item: u32, pushed: u64, played: u64) -> ItemCount {
        ItemCount {
            item,
            pushed,
            played,
            heard: 0,
        }
    }

    /// A count as answered with no latency: all of it heard.
    fn heard_all(item: u32, pushed: u64, played: u64) -> ItemCount {
        ItemCount {
            heard: played,
            ..count(item, pushed, played)
        }
    }

    fn got<T>(mut rx: oneshot::Receiver<T>) -> Option<T> {
        rx.try_recv().ok().flatten()
    }

    #[test]
    fn a_flushed_item_never_takes_audio_again() {
        let mut l = Ledger::default();
        let a = l.begin();
        assert_eq!(l.push(a, &[1, 0, 2, 0]), Push::Post(vec![1, 0, 2, 0]));
        let (id, rx, _) = l.flush(Some(a), 0, 0.0);
        // The deltas still in flight when the barge-in flushed it.
        assert_eq!(l.push(a, &[3, 0]), Push::Late { first: true });
        assert_eq!(l.push(a, &[4, 0, 5]), Push::Late { first: false });
        assert_eq!(l.late(), (2, 5));
        l.on_flushed(id, &[count(a, 2, 1)], 0, 0.0);
        assert_eq!(got(rx).unwrap(), vec![heard_all(a, 2, 1)]);
        // Still closed after the answer.
        assert_eq!(l.push(a, &[6, 0]), Push::Late { first: false });
        // A flush of everything closes every item, the new one takes audio.
        let b = l.begin();
        let (_, _rx, _) = l.flush(None, 0, 0.0);
        assert_eq!(l.push(b, &[7, 0]), Push::Late { first: true });
        let c = l.begin();
        assert_eq!(l.push(c, &[8, 0]), Push::Post(vec![8, 0]));
        // An item never begun takes nothing.
        assert!(matches!(l.push(99, &[1, 0]), Push::Late { .. }));
    }

    #[test]
    fn an_ended_item_takes_no_more_pushes() {
        let mut l = Ledger::default();
        let a = l.begin();
        let (_rx, post) = l.end(a, 0, 0.0);
        assert!(post);
        assert!(matches!(l.push(a, &[1, 0]), Push::Late { .. }));
    }

    #[test]
    fn played_keeps_the_final_count_after_the_item_ended() {
        let mut l = Ledger::default();
        let a = l.begin();
        assert_eq!(l.played(a), Some(0));
        l.on_progress(a, 1200);
        assert_eq!(l.played(a), Some(1200));
        let (rx, _) = l.end(a, 0, 0.0);
        l.on_ended(count(a, 2400, 2400), 0, 10.0);
        assert_eq!(got(rx).unwrap().unwrap().played, 2400);
        assert_eq!(l.played(a), Some(2400), "not 0 once it ended");
        // A flushed one keeps its count too.
        let b = l.begin();
        let (id, _rx, _) = l.flush(Some(b), 0, 0.0);
        l.on_flushed(id, &[count(b, 4800, 960)], 0, 20.0);
        assert_eq!(l.played(b), Some(960));
        l.release(b);
        assert_eq!(l.played(b), None);
        assert_eq!(l.played(77), None);
    }

    #[test]
    fn a_flush_right_after_play_out_answers_the_final_count() {
        let mut l = Ledger::default();
        let a = l.begin();
        let (end_rx, _) = l.end(a, 0, 0.0);
        // The barge-in: the page flushes as the worklet reports the end; the
        // port delivers `ended` first, then the flush's (empty) answer.
        let (id, flush_rx, _) = l.flush(Some(a), 0, 0.0);
        l.on_ended(count(a, 2400, 2400), 0, 5.0);
        l.on_flushed(id, &[], 0, 6.0);
        assert_eq!(
            got(flush_rx).unwrap(),
            vec![heard_all(a, 2400, 2400)],
            "the item's final count, not []"
        );
        assert_eq!(
            got(end_rx).unwrap().map(|c| c.played),
            Some(2400),
            "its end waiter learns it played to the end"
        );
        // Flushing it again later answers the same.
        let (id, rx, _) = l.flush(Some(a), 0, 0.0);
        l.on_flushed(id, &[], 0, 7.0);
        assert_eq!(got(rx).unwrap()[0].played, 2400);
    }

    #[test]
    fn an_end_waiter_gets_none_only_when_audio_was_dropped() {
        let mut l = Ledger::default();
        let a = l.begin();
        let (rx, _) = l.end(a, 0, 0.0);
        let (id, _f, _) = l.flush(None, 0, 0.0);
        l.on_flushed(id, &[count(a, 4800, 100)], 0, 1.0);
        assert_eq!(got(rx), Some(None));
        // `end` after the item closed answers at once, without the worklet.
        let (rx, post) = l.end(a, 0, 2.0);
        assert!(!post);
        assert_eq!(got(rx), Some(None));
        let b = l.begin();
        l.on_ended(count(b, 10, 10), 0, 3.0);
        let (rx, post) = l.end(b, 0, 4.0);
        assert!(!post);
        assert_eq!(got(rx).unwrap().unwrap().played, 10);
    }

    #[test]
    fn a_later_item_closing_supersedes_older_finals() {
        let mut l = Ledger::default();
        let a = l.begin();
        l.on_ended(count(a, 10, 10), 0, 0.0);
        let b = l.begin();
        let c = l.begin();
        // One flush closes two items: both kept.
        let (id, _rx, _) = l.flush(None, 0, 0.0);
        l.on_flushed(id, &[count(b, 5, 1), count(c, 5, 2)], 0, 1.0);
        assert_eq!(l.played(a), None, "superseded by later items");
        assert_eq!(l.played(b), Some(1));
        assert_eq!(l.played(c), Some(2));
        let d = l.begin();
        l.on_ended(count(d, 3, 3), 0, 2.0);
        assert_eq!((l.played(b), l.played(c)), (None, None));
        assert_eq!(l.played(d), Some(3));
    }

    #[test]
    fn a_flush_of_everything_answers_dropped_and_finished_items() {
        let mut l = Ledger::default();
        let a = l.begin();
        let (_e, _) = l.end(a, 0, 0.0);
        let b = l.begin();
        let (id, rx, _) = l.flush(None, 0, 0.0);
        l.on_ended(count(a, 4, 4), 0, 1.0);
        l.on_flushed(id, &[count(b, 9, 3)], 0, 2.0);
        assert_eq!(
            got(rx).unwrap(),
            vec![heard_all(b, 9, 3), heard_all(a, 4, 4)]
        );
    }

    #[test]
    fn heard_takes_off_the_output_latency_still_unplayed() {
        // 2400 samples of latency (100 ms): a flush counts it as unheard.
        assert_eq!(heard(24_000, 2_400, 0.0), 21_600);
        // Half of it played out after an item ended 50 ms ago.
        assert_eq!(heard(24_000, 2_400, 50.0), 22_800);
        assert_eq!(heard(24_000, 2_400, 500.0), 24_000);
        assert_eq!(heard(1_000, 2_400, 0.0), 0, "clamped at 0");
        let mut l = Ledger::default();
        let a = l.begin();
        l.on_progress(a, 24_000);
        assert_eq!(l.heard(a, 2_400, 0.0), Some(21_600));
        let (id, rx, _) = l.flush(Some(a), 0, 0.0);
        l.on_flushed(id, &[count(a, 48_000, 24_000)], 2_400, 1_000.0);
        assert_eq!(got(rx).unwrap()[0].heard, 21_600);
        // A flushed count stays as it was when flushed.
        assert_eq!(l.heard(a, 2_400, 9_000.0), Some(21_600));
        let b = l.begin();
        l.on_ended(count(b, 24_000, 24_000), 2_400, 1_000.0);
        assert_eq!(l.heard(b, 2_400, 1_000.0), Some(21_600));
        assert_eq!(l.heard(b, 2_400, 1_100.0), Some(24_000));
    }

    #[test]
    fn a_flush_the_worklet_did_not_answer_reports_the_last_counts() {
        let mut l = Ledger::default();
        let a = l.begin();
        l.on_progress(a, 480);
        let (id, _rx, _) = l.flush(Some(a), 0, 0.0);
        assert_eq!(
            l.flush_timed_out(id, 0, 0.0),
            vec![ItemCount {
                item: a,
                pushed: 0,
                played: 480,
                heard: 480
            }]
        );
        // The worklet's late answer still closes the item.
        l.on_flushed(id, &[count(a, 960, 500)], 0, 1.0);
        assert_eq!(l.played(a), Some(500));
        assert!(!l.any_live());
    }

    #[test]
    fn progress_of_a_closed_item_is_ignored() {
        let mut l = Ledger::default();
        let a = l.begin();
        l.on_ended(count(a, 10, 10), 0, 0.0);
        l.on_progress(a, 3);
        assert_eq!(l.played(a), Some(10));
        l.on_underrun();
        assert_eq!(l.underruns(), 1);
    }
}
