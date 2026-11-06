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
//!   second time;
//! - **results let in before the page's own turn**: a send lets the MCP
//!   task results that waited in before its message (MCP Tasks design
//!   §3.1), so the rows can be `[result…, message, reply]` where the page
//!   shows `[message, reply]` — its user turn known by the `turn` frame's
//!   id, or still a bubble (a send stopped before it), its reply by
//!   `done`'s or not; an answer, an edit or a regenerate lets them in at
//!   its start, before its reply. The results (rows of role `tool`) go in
//!   front of the message they were stored before, and the page's bubbles
//!   adopt the rows after them ([`in_front`]), rather than the transcript
//!   being loaded afresh. Any other row the page lacks in front of one it
//!   shows still reloads.

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
    /// `adopt` do too and take its id, `drop` (page indices) go, `insert`
    /// (page index, row index) are added in front of that page message, in
    /// order, and `append` (row indices) are added at the end.
    Patch {
        pairs: Vec<(usize, usize)>,
        adopt: Vec<(usize, usize)>,
        drop: Vec<usize>,
        insert: Vec<(usize, usize)>,
        append: Vec<usize>,
    },
    /// The history was rewritten in another order: load the rows afresh.
    Reload,
}

impl Plan {
    /// Nothing is added, dropped or adopted: at most fields change.
    #[cfg(test)]
    fn same_list(&self) -> bool {
        matches!(self, Plan::Patch { adopt, drop, insert, append, .. }
            if adopt.is_empty() && drop.is_empty() && insert.is_empty() && append.is_empty())
    }
}

/// Match `page` to `rows` (`(id, role)`), both in conversation order.
pub(super) fn plan(page: &[Shown<'_>], rows: &[(i64, &str)]) -> Plan {
    let mut pairs = Vec::new();
    let mut adopt = Vec::new();
    let mut insert = Vec::new();
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
        // Before one already matched, or another role: the history moved
        // in a way a patch cannot follow.
        if ri < next || rows[ri].1 != m.role {
            return Plan::Reload;
        }
        // Rows the page lacks in front of it: only results let in before
        // the page's own turn are followed in place.
        if ri > next {
            let Some(front) = in_front(page, tail..pi, pi, rows, next..ri) else {
                return Plan::Reload;
            };
            insert.extend(front.insert);
            adopt.extend(front.adopt);
        }
        pairs.push((pi, ri));
        next = ri + 1;
        tail = pi + 1;
    }
    if pairs.is_empty() && !drop.is_empty() && !rows.is_empty() {
        // Nothing shown is stored any more: the thread is another one now.
        return Plan::Reload;
    }
    // The page's own send after the last matched message, its user turn
    // unconfirmed, with results stored in front of it: they go in front.
    let first = page
        .iter()
        .enumerate()
        .skip(tail)
        .find(|(_, m)| m.id.is_none());
    if let Some((pi, m)) = first.filter(|(_, m)| !m.unsaved && m.role == USER) {
        let results = results_from(rows, next);
        if results > 0 && rows.get(next + results).is_some_and(|r| r.1 == m.role) {
            insert.extend((next..next + results).map(|ri| (pi, ri)));
            next += results;
        }
    }
    // The bubbles after the last matched message whose id the page never
    // heard take the rows at their place, while the roles agree.
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
        insert,
        append: (next..rows.len()).collect(),
    }
}

/// A user turn's role.
const USER: &str = "user";

/// A late MCP task result's role (MCP Tasks design §2.2).
const RESULT: &str = "tool";

/// How many results are stored from row `at` on.
fn results_from(rows: &[(i64, &str)], at: usize) -> usize {
    rows.iter().skip(at).take_while(|r| r.1 == RESULT).count()
}

/// What the page does with stored rows it lacks in front of a message it
/// shows ([`in_front`]).
#[derive(Debug, PartialEq, Eq)]
struct Front {
    insert: Vec<(usize, usize)>,
    adopt: Vec<(usize, usize)>,
}

/// The rows `gap` the page lacks in front of the message it shows at
/// `at`, its own bubbles `bubbles` (page indices) between the last matched
/// message and that one: `Some` when the bubbles not refused take the
/// gap's last rows, role by role, and every row before those is a result,
/// which goes in front of the first of those bubbles — a send's user turn,
/// the only bubble a result is let in before — or, with none, in front of
/// the message at `at` (results are never moved, so a row the page lacks
/// before one it shows entered there). `None` (load afresh) for anything
/// else.
fn in_front(
    page: &[Shown<'_>],
    bubbles: std::ops::Range<usize>,
    at: usize,
    rows: &[(i64, &str)],
    gap: std::ops::Range<usize>,
) -> Option<Front> {
    let waiting: Vec<usize> = bubbles
        .filter(|&pi| page[pi].id.is_none() && !page[pi].unsaved)
        .collect();
    let results = results_from(rows, gap.start).min(gap.len());
    let taken = gap.start + results..gap.end;
    if taken.len() != waiting.len()
        || waiting
            .iter()
            .zip(taken.clone())
            .any(|(&pi, ri)| page[pi].role != rows[ri].1)
    {
        return None;
    }
    let front = match waiting.first() {
        // A bubble takes a row by its role alone: only a send's user turn
        // has results in front of it.
        Some(&first) if results > 0 && page[first].role != USER => return None,
        Some(&first) => first,
        None => at,
    };
    Some(Front {
        insert: (gap.start..gap.start + results)
            .map(|ri| (front, ri))
            .collect(),
        adopt: waiting.into_iter().zip(taken).collect(),
    })
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
            insert: Vec::new(),
            append: append.to_vec(),
        }
    }

    /// A patch that drops nothing and inserts `insert`.
    fn inserting(
        pairs: &[(usize, usize)],
        adopt: &[(usize, usize)],
        insert: &[(usize, usize)],
        append: &[usize],
    ) -> Plan {
        Plan::Patch {
            pairs: pairs.to_vec(),
            adopt: adopt.to_vec(),
            drop: Vec::new(),
            insert: insert.to_vec(),
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

    #[test]
    fn a_send_s_waiting_results_go_in_front_of_its_user_turn() {
        // The send let a result in before its message; the page shows the
        // message (no id yet) and its reply (the id `done` said).
        let page = [
            stored(1, "user"),
            stored(2, "assistant"),
            bubble("user", false),
            stored(5, "assistant"),
        ];
        let rows = [
            (1, "user"),
            (2, "assistant"),
            (3, "tool"),
            (4, "user"),
            (5, "assistant"),
        ];
        assert_eq!(
            plan(&page, &rows),
            inserting(&[(0, 0), (1, 1), (3, 4)], &[(2, 3)], &[(2, 2)], &[])
        );
        // Two results, in their order.
        let rows = [
            (1, "user"),
            (2, "assistant"),
            (3, "tool"),
            (4, "tool"),
            (6, "user"),
            (5, "assistant"),
        ];
        assert_eq!(
            plan(&page, &rows),
            inserting(&[(0, 0), (1, 1), (3, 5)], &[(2, 4)], &[(2, 2), (2, 3)], &[])
        );
        // No result: the user turn is adopted where it is.
        let rows = [(1, "user"), (2, "assistant"), (4, "user"), (5, "assistant")];
        assert_eq!(
            plan(&page, &rows),
            inserting(&[(0, 0), (1, 1), (3, 3)], &[(2, 2)], &[], &[])
        );
    }

    #[test]
    fn a_send_whose_ids_are_known_takes_its_results_in_front() {
        // The `turn` frame named the user row, `done` the reply.
        let page = [
            stored(1, "user"),
            stored(2, "assistant"),
            stored(6, "user"),
            stored(7, "assistant"),
        ];
        let rows = [
            (1, "user"),
            (2, "assistant"),
            (5, "tool"),
            (6, "user"),
            (7, "assistant"),
        ];
        assert_eq!(
            plan(&page, &rows),
            inserting(&[(0, 0), (1, 1), (2, 3), (3, 4)], &[], &[(2, 2)], &[])
        );
        // An answer's or an edit's: let in at its start, before its reply.
        let page = [stored(1, "user"), stored(7, "assistant")];
        let rows = [(1, "user"), (5, "tool"), (7, "assistant")];
        assert_eq!(
            plan(&page, &rows),
            inserting(&[(0, 0), (1, 2)], &[], &[(1, 1)], &[])
        );
    }

    #[test]
    fn a_send_without_a_confirmed_reply_takes_its_results_in_front_too() {
        // Stopped before `done`: neither id heard.
        let page = [
            stored(1, "user"),
            bubble("user", false),
            bubble("assistant", false),
        ];
        let rows = [(1, "user"), (3, "tool"), (4, "user"), (5, "assistant")];
        assert_eq!(
            plan(&page, &rows),
            inserting(&[(0, 0)], &[(1, 2), (2, 3)], &[(1, 1)], &[])
        );
        // A refused reply: the user turn is adopted, the reply stays.
        let page = [
            stored(1, "user"),
            bubble("user", false),
            bubble("assistant", true),
        ];
        let rows = [(1, "user"), (3, "tool"), (4, "user"), (6, "user")];
        assert_eq!(
            plan(&page, &rows),
            inserting(&[(0, 0)], &[(1, 2)], &[(1, 1)], &[3])
        );
    }

    #[test]
    fn rows_in_front_that_are_no_send_s_results_still_load_afresh() {
        // Another writer's turn stored before the page's message.
        let page = [
            stored(1, "user"),
            bubble("user", false),
            stored(7, "assistant"),
        ];
        let rows = [
            (1, "user"),
            (3, "user"),
            (4, "assistant"),
            (5, "user"),
            (7, "assistant"),
        ];
        assert_eq!(plan(&page, &rows), Plan::Reload);
        // A result in front of a reply bubble: a bubble takes rows by role
        // alone, and only a user turn's has results in front of it.
        let page = [
            stored(1, "user"),
            bubble("assistant", false),
            stored(9, "user"),
        ];
        let rows = [(1, "user"), (3, "tool"), (4, "assistant"), (9, "user")];
        assert_eq!(plan(&page, &rows), Plan::Reload);
    }
}
