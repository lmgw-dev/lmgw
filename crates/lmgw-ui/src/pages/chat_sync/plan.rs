//! How the open thread's messages become the stored rows another writer
//! left (`chat_sync`): matched by id and patched in place, the ones the
//! store lost dropped, the new ones added at the end.
//!
//! The voice session's read-back has a plan of its own
//! (`chat_voice::realtime::reload`): there the page's bubbles without a row
//! are the session's own refused or removed turns, and go. Here they are
//! the page's own and stay:
//! - **an unsaved reply** (the server refused to store it) keeps its place,
//!   dimmed, with Copy;
//! - **a reply whose id the page never heard** (a Stop aborts the stream
//!   before its `done`, though the gateway saved what it had) takes the
//!   stored row at its place when the roles agree — adopted, not added a
//!   second time.

/// One message of the page, as the plan reads it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Shown<'a> {
    /// The stored row's id, once the page knows it.
    pub id: Option<i64>,
    pub role: &'a str,
    /// A reply the server refused to store.
    pub unsaved: bool,
}

/// What to do with the page's messages.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// In place: `pairs` (page index, row index) take the row's fields,
    /// `adopt` do too and take its id, `drop` (page indices) go, `append`
    /// (row indices) are added at the end.
    Patch {
        pairs: Vec<(usize, usize)>,
        adopt: Vec<(usize, usize)>,
        drop: Vec<usize>,
        append: Vec<usize>,
    },
    /// The history was rewritten in another order: load the rows afresh.
    Reload,
}

impl Plan {
    /// Nothing is added, dropped or adopted: at most fields change.
    #[cfg(test)]
    fn same_list(&self) -> bool {
        matches!(self, Plan::Patch { adopt, drop, append, .. }
            if adopt.is_empty() && drop.is_empty() && append.is_empty())
    }
}

/// Match `page` to `rows` (`(id, role)`), both in conversation order.
pub(super) fn plan(page: &[Shown<'_>], rows: &[(i64, &str)]) -> Plan {
    let mut pairs = Vec::new();
    let mut drop = Vec::new();
    let mut next = 0usize;
    let mut tail = 0usize;
    for (pi, m) in page.iter().enumerate() {
        let Some(id) = m.id else {
            // The page's own bubble: it stays where it is.
            continue;
        };
        let Some(ri) = rows.iter().position(|(r, _)| *r == id) else {
            // Deleted, or cut away with a rewrite, by another writer.
            drop.push(pi);
            continue;
        };
        // Before one already matched, another role, or a row the page lacks
        // in front of it: the history moved in a way a patch cannot follow.
        if ri != next || rows[ri].1 != m.role {
            return Plan::Reload;
        }
        pairs.push((pi, ri));
        next = ri + 1;
        tail = pi + 1;
    }
    if pairs.is_empty() && !drop.is_empty() && !rows.is_empty() {
        // Nothing shown is stored any more: the thread is another one now.
        return Plan::Reload;
    }
    // The bubbles after the last matched message whose id the page never
    // heard take the rows at their place, while the roles agree.
    let mut adopt = Vec::new();
    for (pi, m) in page.iter().enumerate().skip(tail) {
        if m.id.is_some() {
            // Dropped above.
            continue;
        }
        if m.unsaved || next >= rows.len() || rows[next].1 != m.role {
            break;
        }
        adopt.push((pi, next));
        next += 1;
    }
    Plan::Patch {
        pairs,
        adopt,
        drop,
        append: (next..rows.len()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(id: i64, role: &str) -> Shown<'_> {
        Shown {
            id: Some(id),
            role,
            unsaved: false,
        }
    }

    fn bubble(role: &str, unsaved: bool) -> Shown<'_> {
        Shown {
            id: None,
            role,
            unsaved,
        }
    }

    fn patch(
        pairs: &[(usize, usize)],
        adopt: &[(usize, usize)],
        drop: &[usize],
        append: &[usize],
    ) -> Plan {
        Plan::Patch {
            pairs: pairs.to_vec(),
            adopt: adopt.to_vec(),
            drop: drop.to_vec(),
            append: append.to_vec(),
        }
    }

    #[test]
    fn the_page_s_own_turn_read_back_changes_no_list() {
        let page = [stored(1, "user"), stored(2, "assistant")];
        let p = plan(&page, &[(1, "user"), (2, "assistant")]);
        assert!(p.same_list(), "{p:?}");
        assert!(plan(&[], &[]).same_list());
    }

    #[test]
    fn another_writer_s_turn_is_added_at_the_end() {
        let page = [stored(1, "user"), stored(2, "assistant")];
        // A device's user turn, stored before its reply.
        let rows = [(1, "user"), (2, "assistant"), (3, "user")];
        assert_eq!(plan(&page, &rows), patch(&[(0, 0), (1, 1)], &[], &[], &[2]));
        // An empty thread takes every row.
        assert_eq!(
            plan(&[], &[(1, "user"), (2, "assistant")]),
            patch(&[], &[], &[], &[0, 1])
        );
    }

    #[test]
    fn a_message_another_writer_deleted_goes() {
        let page = [
            stored(1, "user"),
            stored(2, "assistant"),
            stored(3, "user"),
            stored(4, "assistant"),
        ];
        assert_eq!(
            plan(&page, &[(1, "user"), (3, "user"), (4, "assistant")]),
            patch(&[(0, 0), (2, 1), (3, 2)], &[], &[1], &[])
        );
        // An edit elsewhere: the user turn rewritten, the answers after it
        // cut, and a new answer.
        assert_eq!(
            plan(
                &page,
                &[(1, "user"), (2, "assistant"), (3, "user"), (9, "assistant")]
            ),
            patch(&[(0, 0), (1, 1), (2, 2)], &[], &[3], &[3])
        );
    }

    #[test]
    fn the_page_s_own_bubbles_stay() {
        // A refused reply: no row, and the rows another writer added go
        // after it.
        let page = [stored(1, "user"), bubble("assistant", true)];
        assert_eq!(
            plan(&page, &[(1, "user"), (5, "user"), (6, "assistant")]),
            patch(&[(0, 0)], &[], &[], &[1, 2])
        );
        // A stopped reply the gateway saved: its row is adopted, not added.
        let page = [stored(1, "user"), bubble("assistant", false)];
        assert_eq!(
            plan(&page, &[(1, "user"), (2, "assistant")]),
            patch(&[(0, 0)], &[(1, 1)], &[], &[])
        );
        // One it did not save stays, without a row.
        assert_eq!(plan(&page, &[(1, "user")]), patch(&[(0, 0)], &[], &[], &[]));
        // A bubble between stored messages is kept where it is.
        let page = [
            stored(1, "user"),
            bubble("assistant", true),
            stored(3, "user"),
        ];
        assert_eq!(
            plan(&page, &[(1, "user"), (3, "user")]),
            patch(&[(0, 0), (2, 1)], &[], &[], &[])
        );
    }

    #[test]
    fn a_history_in_another_order_loads_afresh() {
        let rows = [(1, "user"), (2, "assistant"), (3, "user")];
        // A row between two shown ones.
        assert_eq!(
            plan(&[stored(1, "user"), stored(3, "user")], &rows),
            Plan::Reload
        );
        // Rows before the first one shown.
        assert_eq!(plan(&[stored(2, "assistant")], &rows), Plan::Reload);
        // Swapped.
        assert_eq!(
            plan(&[stored(2, "assistant"), stored(1, "user")], &rows),
            Plan::Reload
        );
        // A role that differs.
        assert_eq!(plan(&[stored(1, "assistant")], &rows), Plan::Reload);
        // Nothing shown is stored any more.
        assert_eq!(plan(&[stored(7, "user")], &rows), Plan::Reload);
    }
}
