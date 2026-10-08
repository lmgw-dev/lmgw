//! The follower's decisions on plain values, so they are tested off the DOM
//! (review CL-5, CL-6's UI gaps, CL-18): what a read of the open thread that
//! came back does, what a stored row of the open thread changes on the page,
//! and how the model picker's picks reach the server. `chat_sync` and the
//! page apply them to signals.

use super::super::chat::ChatThread;

/// What a read of the open thread that came back does (review CL-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::pages) enum Landed {
    /// Another thread is open now, or none: it is dropped.
    Left,
    /// The page's own reply streams into it, or a voice session holds its
    /// transcript: it waits, and runs once that ends.
    Wait,
    /// The page changed the transcript while it was out, so it may be older
    /// than what the page shows: it is dropped and made again.
    Again,
    /// It is taken.
    Take,
}

/// What a read of thread `id` does that went out when the page's own
/// transcript changes counted `sent_at`, now that thread `open` is open,
/// the page's own work does (`busy`) or does not hold it, and the count is
/// `now` (`None` once the page is gone).
pub(in crate::pages) fn landed(
    id: i64,
    open: Option<i64>,
    busy: bool,
    sent_at: u64,
    now: Option<u64>,
) -> Landed {
    if open != Some(id) || now.is_none() {
        Landed::Left
    } else if busy {
        Landed::Wait
    } else if now != Some(sent_at) {
        Landed::Again
    } else {
        Landed::Take
    }
}

/// What the stored row of the open thread changes on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::pages) enum RowTake {
    /// It is the row the page holds.
    Same,
    /// Only its time moved (its messages moved it): taken without a
    /// re-render.
    OnlyWhen,
    /// Something shown moved.
    Changed {
        /// The settings form is re-seeded from it (the owner is not editing
        /// the form); else the form is rebased onto it, so the fields the
        /// owner did not touch follow and the edits stay.
        reseed: bool,
        /// The picker follows to the stored model: it showed the model the
        /// page held, which moved, and no pick of the owner's for the
        /// thread is on its way. A picker showing another model holds such
        /// a pick; so may one showing the model held (a pick back to it,
        /// waiting for the save before it, review CL-15): that save's answer
        /// and its frame settle the picker.
        follow_model: bool,
    },
}

/// What the stored row `stored` changes, where the page holds `held`, the
/// owner is (`form_edited`) or is not editing the settings form, the
/// picker shows `picker`, and a pick of the owner's for the thread is
/// (`picking`, [`ModelPicks::busy`]) or is not on its way.
pub(in crate::pages) fn row_take(
    held: &ChatThread,
    stored: &ChatThread,
    form_edited: bool,
    picker: &str,
    picking: bool,
) -> RowTake {
    if held == stored {
        return RowTake::Same;
    }
    let but_when = ChatThread {
        updated_at: held.updated_at.clone(),
        ..stored.clone()
    };
    if *held == but_when {
        return RowTake::OnlyWhen;
    }
    let model_moved = held.model_alias != stored.model_alias;
    RowTake::Changed {
        reseed: !form_edited,
        follow_model: model_moved && picker == held.model_alias && !picking,
    }
}

/// The owner's model picks on their way to the server (review CL-5).
///
/// - **One save at a time, in the order picked**: a pick made while a save
///   is out waits for its answer, so the last pick is the one stored, in
///   whatever order the server would have taken two saves that overlapped.
///   A thread keeps only its newest waiting pick.
/// - **The picker set from the stored row is no pick** ([`Self::follow`]):
///   a model another writer chose is never saved back. Marked explicitly,
///   not inferred from what the page holds: a pick back to the model the
///   page holds, made while the save of another pick is out, is a pick.
#[derive(Debug, Default)]
pub(in crate::pages) struct ModelPicks {
    /// The thread whose picker was set from its stored row, and the model.
    followed: Option<(i64, String)>,
    /// The thread whose pick's save is out.
    out: Option<i64>,
    /// The picks made while a save was out, oldest first, one per thread.
    waiting: Vec<(i64, String)>,
}

impl ModelPicks {
    /// The picker of thread `tid` is set to `model` from its stored row.
    pub(in crate::pages) fn follow(&mut self, tid: i64, model: &str) {
        self.followed = Some((tid, model.to_string()));
    }

    /// The picker of thread `tid` moved to `model`: the save to send now,
    /// if any.
    pub(in crate::pages) fn picked(&mut self, tid: i64, model: &str) -> Option<(i64, String)> {
        let followed = self.followed.take();
        if followed.is_some_and(|(t, m)| t == tid && m == model) {
            return None;
        }
        if self.out.is_some() {
            self.waiting.retain(|(t, _)| *t != tid);
            self.waiting.push((tid, model.to_string()));
            return None;
        }
        self.out = Some(tid);
        Some((tid, model.to_string()))
    }

    /// The save that was out answered, taken or refused: the next to send.
    pub(in crate::pages) fn answered(&mut self) -> Option<(i64, String)> {
        if self.waiting.is_empty() {
            self.out = None;
            return None;
        }
        let next = self.waiting.remove(0);
        self.out = Some(next.0);
        Some(next)
    }

    /// A pick for thread `tid` is on its way: its save is out, or it waits
    /// for the one out (review CL-15).
    pub(in crate::pages) fn busy(&self, tid: i64) -> bool {
        self.out == Some(tid) || self.waiting.iter().any(|(t, _)| *t == tid)
    }

    /// Another thread opened: a follow of the one left is void.
    pub(in crate::pages) fn left(&mut self) {
        self.followed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(model: &str) -> ChatThread {
        ChatThread {
            id: 7,
            model_alias: model.into(),
            updated_at: "2026-10-08 10:00:00".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_row_whose_time_alone_moved_changes_nothing_shown() {
        let held = thread("a");
        assert_eq!(row_take(&held, &held, false, "a", false), RowTake::Same);
        let later = ChatThread {
            updated_at: "2026-10-08 10:00:05".into(),
            ..held.clone()
        };
        assert_eq!(
            row_take(&held, &later, false, "a", false),
            RowTake::OnlyWhen
        );
    }

    #[test]
    fn the_form_reseeds_unless_edited_and_the_picker_follows_only_the_model_held() {
        let held = thread("a");
        let renamed = ChatThread {
            title: "elsewhere".into(),
            ..held.clone()
        };
        assert_eq!(
            row_take(&held, &renamed, false, "a", false),
            RowTake::Changed {
                reseed: true,
                follow_model: false
            }
        );
        assert_eq!(
            row_take(&held, &renamed, true, "a", false),
            RowTake::Changed {
                reseed: false,
                follow_model: false
            }
        );
        let moved = thread("b");
        // The picker shows the model held: it follows.
        assert_eq!(
            row_take(&held, &moved, false, "a", false),
            RowTake::Changed {
                reseed: true,
                follow_model: true
            }
        );
        // It shows a pick of the owner's on its way: it stays.
        assert_eq!(
            row_take(&held, &moved, false, "c", false),
            RowTake::Changed {
                reseed: true,
                follow_model: false
            }
        );
    }

    /// Review CL-15: on A, the owner picks B (its save out), then back to A
    /// (waiting). A read that saw B's commit lands before B's answer: the
    /// picker shows the model held (A), the stored row says B. It does not
    /// follow to B: A's save is on its way, and its answer settles it.
    #[test]
    fn the_picker_does_not_follow_while_a_pick_for_the_thread_is_on_its_way() {
        let held = thread("a");
        let stored = thread("b");
        let mut p = ModelPicks::default();
        assert!(p.picked(7, "b").is_some());
        assert_eq!(p.picked(7, "a"), None);
        assert!(p.busy(7) && !p.busy(8));
        assert_eq!(
            row_take(&held, &stored, false, "a", p.busy(7)),
            RowTake::Changed {
                reseed: true,
                follow_model: false
            }
        );
        // B answered, A's save is out: still on its way.
        assert_eq!(p.answered(), Some((7, "a".into())));
        assert!(p.busy(7));
        // A answered: nothing more on its way, so a model another writer
        // sets is followed again.
        assert_eq!(p.answered(), None);
        assert!(!p.busy(7));
        assert_eq!(
            row_take(&held, &stored, false, "a", p.busy(7)),
            RowTake::Changed {
                reseed: true,
                follow_model: true
            }
        );
        // A save out for another thread is no reason here.
        assert!(p.picked(8, "x").is_some());
        assert!(!p.busy(7) && p.busy(8));
    }

    /// The review's scenario: on A, pick B, then back to A before B's save
    /// answered. A is saved after B, so A is stored, and shown.
    #[test]
    fn a_pick_back_while_a_save_is_out_is_saved_after_it() {
        let mut p = ModelPicks::default();
        assert_eq!(p.picked(7, "b"), Some((7, "b".into())));
        assert_eq!(p.picked(7, "a"), None, "waits for B's answer");
        assert_eq!(p.answered(), Some((7, "a".into())), "then A goes");
        assert_eq!(p.answered(), None, "nothing more");
        assert_eq!(p.picked(7, "c"), Some((7, "c".into())), "idle again");
    }

    #[test]
    fn a_thread_keeps_its_newest_waiting_pick_and_each_thread_its_own() {
        let mut p = ModelPicks::default();
        assert!(p.picked(7, "b").is_some());
        assert_eq!(p.picked(7, "c"), None);
        assert_eq!(p.picked(8, "x"), None);
        assert_eq!(p.picked(7, "d"), None);
        assert_eq!(p.answered(), Some((8, "x".into())));
        assert_eq!(p.answered(), Some((7, "d".into())));
        assert_eq!(p.answered(), None);
    }

    #[test]
    fn a_followed_model_is_no_pick_and_a_follow_is_used_once() {
        let mut p = ModelPicks::default();
        p.follow(7, "b");
        assert_eq!(p.picked(7, "b"), None, "set from the stored row");
        assert_eq!(p.picked(7, "a"), Some((7, "a".into())), "the owner's");
        assert_eq!(p.answered(), None);
        // A pick that came before the follow was applied is a pick.
        p.follow(7, "b");
        assert_eq!(p.picked(7, "c"), Some((7, "c".into())));
        assert_eq!(p.answered(), None);
        // Another thread's follow, or one left behind, is no excuse.
        p.follow(8, "b");
        assert_eq!(p.picked(7, "b"), Some((7, "b".into())));
        assert_eq!(p.answered(), None);
        p.follow(7, "e");
        p.left();
        assert_eq!(p.picked(7, "e"), Some((7, "e".into())));
    }

    /// Review CL-3, CL-18: a read of the open thread that went out before
    /// the page's own turn, edit or delete changed the transcript, and
    /// answers after it, is made again, never taken.
    #[test]
    fn a_read_older_than_the_page_s_own_change_is_made_again() {
        assert_eq!(landed(7, Some(7), false, 3, Some(3)), Landed::Take);
        assert_eq!(landed(7, Some(7), false, 3, Some(4)), Landed::Again);
        assert_eq!(landed(7, Some(7), true, 3, Some(4)), Landed::Wait);
        assert_eq!(landed(7, Some(7), true, 3, Some(3)), Landed::Wait);
        assert_eq!(landed(7, Some(8), false, 3, Some(3)), Landed::Left);
        assert_eq!(landed(7, None, false, 3, Some(3)), Landed::Left);
        assert_eq!(
            landed(7, Some(7), false, 3, None),
            Landed::Left,
            "page gone"
        );
    }
}
