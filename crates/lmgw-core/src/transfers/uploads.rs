//! One upload slot per transfer (device transfers design §4, I9): whoever
//! holds it is the only one that writes, completes, ends or deletes the
//! transfer's file. Taking it raises the slot's generation first and says
//! who asks ([`Taker`]), which the holder hears ([`Held::preempted`]) and
//! stops at: a retried upload stops the one its client gave up on, and a
//! cancel stops the upload it cancels. Then the taker waits for the holder
//! to let go. The sweep's completion of a full file takes a slot only when
//! it is free (`try_take`), and stops nobody (2026-10-22).
//!
//! A slot lives while anyone holds it or waits for it (2026-10-22, I24):
//! the last of them to let go removes it from the map, so an end with a
//! taker queued behind it, a request that gave up while it waited and an
//! upload that found its transfer overdue leave nothing behind.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{watch, Mutex, OwnedMutexGuard};

/// Who asks for a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Taker {
    /// An upload, or the sweep completing one whose file is full (§4).
    Upload,
    /// An end of the transfer, or the sweep deleting a stray file: what
    /// it stops answers with the state the end leaves (I23).
    End,
}

/// The slots, by transfer id.
#[derive(Debug, Default)]
pub(crate) struct Uploads {
    slots: Arc<StdMutex<HashMap<String, Arc<Slot>>>>,
}

#[derive(Debug)]
struct Slot {
    lock: Arc<Mutex<()>>,
    /// The generation, raised by every taker, and who raised it last.
    asked: watch::Sender<(u64, Taker)>,
}

/// A share of slot `id`: held by every holder and waiter. Dropped, the
/// last share removes the slot from the map; the map is locked whenever a
/// share is made or let go, so the count it reads is the real one.
struct Share {
    slots: Arc<StdMutex<HashMap<String, Arc<Slot>>>>,
    id: String,
    slot: Option<Arc<Slot>>,
}

impl Drop for Share {
    fn drop(&mut self) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = self.slot.take() {
            // The map's and this one: nobody else holds or waits.
            if Arc::strong_count(&slot) == 2
                && slots.get(&self.id).is_some_and(|s| Arc::ptr_eq(s, &slot))
            {
                slots.remove(&self.id);
            }
            drop(slot);
        }
    }
}

/// A slot held: let go when dropped. The guard goes before the share, so
/// the slot is free before its share is counted out.
pub(crate) struct Held {
    _guard: OwnedMutexGuard<()>,
    generation: u64,
    watch: watch::Receiver<(u64, Taker)>,
    _share: Share,
}

impl Held {
    /// Resolves once someone else asked for the slot, saying who: the
    /// holder stops writing and lets go.
    pub(crate) async fn preempted(&mut self) -> Taker {
        loop {
            let (generation, who) = *self.watch.borrow_and_update();
            if generation != self.generation {
                return who;
            }
            if self.watch.changed().await.is_err() {
                // The slot is gone, and no one can ask for it (the share
                // this holds keeps it, so this does not happen).
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Uploads {
    /// A share of transfer `id`'s slot, made when there is none.
    fn share(&self, id: &str) -> Share {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = slots
            .entry(id.to_string())
            .or_insert_with(|| {
                Arc::new(Slot {
                    lock: Arc::new(Mutex::new(())),
                    asked: watch::Sender::new((0, Taker::Upload)),
                })
            })
            .clone();
        Share {
            slots: self.slots.clone(),
            id: id.to_string(),
            slot: Some(slot),
        }
    }

    /// Take transfer `id`'s slot as `who`: the one that holds it now is
    /// asked to let go, and this waits until it did. A taker dropped while
    /// it waits gives up its share.
    pub(crate) async fn take(&self, id: &str, who: Taker) -> Held {
        let share = self.share(id);
        // No second handle on the slot outside the share: the count the
        // last share reads is the shares'.
        let (generation, watch, lock) = {
            let slot = share.slot.as_ref().expect("a fresh share holds its slot");
            let mut generation = 0;
            slot.asked.send_modify(|(g, by)| {
                *g = g.wrapping_add(1);
                *by = who;
                generation = *g;
            });
            (generation, slot.asked.subscribe(), slot.lock.clone())
        };
        let guard = lock.lock_owned().await;
        Held {
            _guard: guard,
            generation,
            watch,
            _share: share,
        }
    }

    /// Take transfer `id`'s slot only when nobody holds it or waits for it,
    /// asking nobody to stop (2026-10-22): the sweep's completion of a full
    /// file, which leaves one an upload writes to that upload (I25). `None`
    /// when it is taken.
    pub(crate) fn try_take(&self, id: &str) -> Option<Held> {
        let share = self.share(id);
        let lock = share.slot.as_ref()?.lock.clone();
        // A lock that is let go passes to its first waiter, so a lock had
        // here has none.
        let guard = lock.try_lock_owned().ok()?;
        let (generation, watch) = {
            let slot = share.slot.as_ref()?;
            (slot.asked.borrow().0, slot.asked.subscribe())
        };
        Some(Held {
            _guard: guard,
            generation,
            watch,
            _share: share,
        })
    }

    /// Wait, asking nobody to stop, until transfer `id`'s slot is free:
    /// what an upload an end stopped waits for before it reads the state
    /// that end left (I23). An end holds the slot for one write and an
    /// unlink.
    pub(crate) async fn settled(&self, id: &str) {
        let share = self.share(id);
        if let Some(lock) = share.slot.as_ref().map(|s| s.lock.clone()) {
            drop(lock.lock_owned().await);
        }
        drop(share);
    }

    /// How many slots are kept: one per transfer someone holds or waits
    /// for, none otherwise.
    pub(crate) fn len(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A second taker stops the first, says who it is, and gets the slot
    /// once the first let go; the slot goes with the last of them.
    #[tokio::test]
    async fn a_newer_taker_stops_the_holder() {
        let uploads = Arc::new(Uploads::default());
        let mut first = uploads.take("t", Taker::Upload).await;
        let u = uploads.clone();
        let second = tokio::spawn(async move { u.take("t", Taker::End).await });
        let who = tokio::time::timeout(Duration::from_secs(5), first.preempted())
            .await
            .expect("the holder hears the second taker");
        assert_eq!(who, Taker::End);
        assert!(!second.is_finished(), "it waits for the holder");
        drop(first);
        let mut second = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap();
        // Nobody asked since: it is not preempted.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.preempted())
                .await
                .is_err()
        );
        assert_eq!(uploads.len(), 1);
        drop(second);
        assert_eq!(uploads.len(), 0);
    }

    /// An end with a taker queued behind it leaves no slot once both let
    /// go, and neither does a taker that gave up while it waited.
    #[tokio::test]
    async fn no_slot_outlives_its_holders_and_waiters() {
        let uploads = Arc::new(Uploads::default());
        let held = uploads.take("t", Taker::End).await;
        let u = uploads.clone();
        let queued = tokio::spawn(async move { u.take("t", Taker::Upload).await });
        let u = uploads.clone();
        let gave_up = tokio::spawn(async move { u.take("t", Taker::Upload).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        gave_up.abort();
        let _ = gave_up.await;
        drop(held);
        let queued = tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uploads.len(), 1, "held by the queued taker");
        drop(queued);
        assert_eq!(uploads.len(), 0);
        // `settled` on a free slot returns at once and keeps nothing.
        tokio::time::timeout(Duration::from_secs(5), uploads.settled("t"))
            .await
            .unwrap();
        assert_eq!(uploads.len(), 0);
    }

    /// `try_take` passes over a slot someone holds without asking it to
    /// stop; a free one it takes, and a later taker stops it as any holder.
    #[tokio::test]
    async fn try_take_passes_over_a_held_slot() {
        let uploads = Arc::new(Uploads::default());
        let mut held = uploads.take("t", Taker::Upload).await;
        assert!(uploads.try_take("t").is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), held.preempted())
                .await
                .is_err(),
            "nobody asked the holder"
        );
        drop(held);
        assert_eq!(uploads.len(), 0);
        let mut swept = uploads.try_take("t").expect("a free slot");
        let u = uploads.clone();
        let upload = tokio::spawn(async move { u.take("t", Taker::Upload).await });
        let who = tokio::time::timeout(Duration::from_secs(5), swept.preempted())
            .await
            .expect("a later taker asks");
        assert_eq!(who, Taker::Upload);
        drop(swept);
        drop(upload.await.unwrap());
        assert_eq!(uploads.len(), 0);
    }

    /// `settled` waits for the holder without asking it to stop.
    #[tokio::test]
    async fn settled_waits_without_asking() {
        let uploads = Arc::new(Uploads::default());
        let mut held = uploads.take("t", Taker::End).await;
        let u = uploads.clone();
        let waiting = tokio::spawn(async move { u.settled("t").await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), held.preempted())
                .await
                .is_err(),
            "nobody asked the holder"
        );
        assert!(!waiting.is_finished());
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uploads.len(), 0);
    }
}
