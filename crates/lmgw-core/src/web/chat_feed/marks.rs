//! Which threads' messages changed, in process: what the dashboard's `chat`
//! frame names as `messages` ([`super::dashboard`]).
//!
//! The change feed records threads and folders, not messages: `message.*`
//! is the Android work package's (§2.2, WP10), and until it records them a
//! message write leaves nothing in the table. Every message write of a
//! stored thread goes through the Chat's repository seam
//! (`web::chat_repo`), which marks its thread here once the write went
//! through. A mark is never stored and never reaches a device's feed.
//!
//! A mark is a generation per thread, not a queue: a reader remembers the
//! generation it read up to and asks for the threads marked after it, so a
//! reader that is slow, or busy for a while, misses nothing and holds no
//! backlog. One entry per stored thread whose messages were written since
//! the gateway started; a deleted thread's entry goes with it
//! ([`Marks::forget`]), since the delete's own record names the thread to
//! every reader (`thread.deleted`).

use std::collections::BTreeMap;

/// The marks: the newest generation, and each marked thread's last one.
#[derive(Debug, Default)]
pub(crate) struct Marks {
    gen: u64,
    threads: BTreeMap<i64, u64>,
}

impl Marks {
    /// Thread `thread_id`'s messages changed.
    pub(super) fn mark(&mut self, thread_id: i64) {
        self.gen += 1;
        self.threads.insert(thread_id, self.gen);
    }

    /// Threads `ids` were deleted: their marks go. A reader that had not
    /// read one yet loses nothing, since the delete's record in the change
    /// feed names the thread. The generation does not move.
    pub(super) fn forget(&mut self, ids: &[i64]) -> bool {
        let before = self.threads.len();
        for id in ids {
            self.threads.remove(id);
        }
        self.threads.len() != before
    }

    /// The newest generation: a reader that starts now reads after it.
    pub(super) fn now(&self) -> u64 {
        self.gen
    }

    /// The threads marked after generation `seen`, ascending, and the
    /// generation they bring the reader to.
    pub(super) fn since(&self, seen: u64) -> (Vec<i64>, u64) {
        let threads = self
            .threads
            .iter()
            .filter(|(_, g)| **g > seen)
            .map(|(id, _)| *id)
            .collect();
        (threads, self.gen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_hears_each_thread_marked_after_it_read_once() {
        let mut m = Marks::default();
        m.mark(4);
        let start = m.now();
        m.mark(9);
        m.mark(2);
        m.mark(9);
        let (threads, seen) = m.since(start);
        assert_eq!(threads, vec![2, 9]);
        assert_eq!(m.since(seen), (vec![], seen), "nothing new since");
        m.mark(4);
        assert_eq!(m.since(seen).0, vec![4]);
        assert_eq!(m.since(0).0, vec![2, 4, 9], "a reader from the start");
    }

    #[test]
    fn a_deleted_thread_s_mark_goes() {
        let mut m = Marks::default();
        m.mark(4);
        m.mark(9);
        let gen = m.now();
        assert!(m.forget(&[9, 12]));
        assert!(!m.forget(&[9]), "nothing left to forget");
        assert_eq!(m.now(), gen, "forgetting moves no generation");
        assert_eq!(m.since(0).0, vec![4]);
        assert_eq!(m.threads.len(), 1, "no entry piles up for a deleted thread");
    }
}
