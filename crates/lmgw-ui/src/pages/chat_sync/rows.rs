//! A frame that names only messages (review CL-11): a message written moves
//! its thread's `updated_at` and `last_message_at`, and nothing else the
//! list shows. So only the named threads' rows are read
//! (`GET /chat/api/threads/rows?ids=`, the list's own rows, the owner's),
//! taken into the list, and the list is sorted again as the server sorts
//! it ([`order`]). During a device's voice session that is a few rows per
//! turn instead of the whole list with every thread's settings.
//!
//! The whole list is read instead when the rows cannot simply be taken:
//! - a named thread is not in the answer (deleted meanwhile, or out of
//!   reach), or the read failed;
//! - a row belongs in the list shown and is not in it, or is in it and no
//!   longer belongs (a thread archived or restored names its row anyway,
//!   so that is a frame that reads the whole list already);
//! - a read of the whole list is out: this read's ticket would drop its
//!   answer, and with it the changes it brings beyond these rows.
//!
//! A named row that belongs in neither (an active thread while the archived
//! list shows) changes nothing.

use std::cmp::Ordering;

use leptos::prelude::*;
use serde::Deserialize;

use super::super::chat::ChatThread;
use super::ListRead;

/// `GET /chat/api/threads/rows`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Rows {
    threads: Vec<ChatThread>,
}

/// The list's order, the server's: the archived list by `archived_at`,
/// newest first; the active one pinned first, then by `updated_at`, newest
/// first; ties by id, the newest first.
pub(super) fn order(a: &ChatThread, b: &ChatThread, archived: bool) -> Ordering {
    let by_id = b.id.cmp(&a.id);
    if archived {
        return b.archived_at.cmp(&a.archived_at).then(by_id);
    }
    b.pinned
        .cmp(&a.pinned)
        .then_with(|| b.updated_at.cmp(&a.updated_at))
        .then(by_id)
}

/// What the rows read for `named` do to the list `shown`.
#[derive(Debug, PartialEq)]
pub(super) enum Merge {
    /// Nothing shown changed.
    Same,
    /// The list with the rows taken, sorted again.
    List(Vec<ChatThread>),
    /// They cannot simply be taken (module doc): read the whole list.
    Whole,
}

/// `rows`, read for the threads `named`, taken into the list `shown` (the
/// archived one when `archived`).
pub(super) fn merge(
    shown: &[ChatThread],
    named: &[i64],
    rows: &[ChatThread],
    archived: bool,
) -> Merge {
    let mut next = shown.to_vec();
    for id in named {
        let Some(row) = rows.iter().find(|r| r.id == *id) else {
            return Merge::Whole;
        };
        let belongs = row.archived_at.is_some() == archived;
        match (belongs, next.iter().position(|t| t.id == *id)) {
            (true, Some(i)) => next[i] = row.clone(),
            (false, None) => {}
            _ => return Merge::Whole,
        }
    }
    next.sort_by(|a, b| order(a, b, archived));
    if next == shown {
        Merge::Same
    } else {
        Merge::List(next)
    }
}

/// Read the rows of `named` and take them into the list (module doc).
pub(super) async fn read(list: ListRead, named: Vec<i64>) {
    if !super::reads::rows_alone(list.out.try_get_value().unwrap_or(0)) {
        list.read().await;
        return;
    }
    let Some(ticket) = list.latest.next() else {
        return;
    };
    let archived = list.view_archived.get_untracked();
    let ids = named
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let got = crate::api::get::<Rows>(format!("/chat/api/threads/rows?ids={ids}")).await;
    // A newer read went out meanwhile (the whole list, or the toolbar's
    // other one): it is the list.
    if !list.latest.is(ticket) {
        return;
    }
    let merged = match got {
        Ok(r) => list
            .threads
            .with_untracked(|shown| merge(shown, &named, &r.threads, archived)),
        Err(e) => {
            leptos::logging::warn!("reading the changed conversations' rows failed: {e}");
            Merge::Whole
        }
    };
    match merged {
        Merge::Same => {}
        Merge::List(v) => list.threads.set(v),
        Merge::Whole => list.read().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: i64, at: &str) -> ChatThread {
        ChatThread {
            id,
            title: format!("t{id}"),
            updated_at: format!("2026-10-08 10:00:{at}"),
            ..Default::default()
        }
    }

    fn ids(v: &[ChatThread]) -> Vec<i64> {
        v.iter().map(|t| t.id).collect()
    }

    /// The owner's case: a device's turn moves an older thread up.
    #[test]
    fn a_named_row_is_taken_and_the_list_sorted_as_the_server_sorts_it() {
        let pinned = ChatThread {
            pinned: true,
            ..t(1, "00")
        };
        let shown = vec![pinned.clone(), t(4, "30"), t(3, "20"), t(2, "20")];
        // Thread 2 has a new message: its time moved past 4's.
        let moved = t(2, "40");
        let Merge::List(next) = merge(&shown, &[2], std::slice::from_ref(&moved), false) else {
            panic!("the list changes");
        };
        assert_eq!(ids(&next), vec![1, 2, 4, 3], "pinned first, then newest");
        assert_eq!(next[1], moved, "the row taken whole");
        // The same second as 3: the newer id first, as the server does.
        let tie = vec![t(4, "30"), t(3, "20")];
        let Merge::List(next) = merge(&tie, &[3], &[t(3, "30")], false) else {
            panic!("the row changes");
        };
        assert_eq!(ids(&next), vec![4, 3]);
    }

    #[test]
    fn a_row_read_again_as_it_was_changes_nothing() {
        let shown = vec![t(4, "30"), t(3, "20")];
        assert_eq!(merge(&shown, &[3], &[t(3, "20")], false), Merge::Same);
    }

    #[test]
    fn a_named_row_gone_or_out_of_place_reads_the_whole_list() {
        let shown = vec![t(4, "30"), t(3, "20")];
        // Deleted meanwhile: not in the answer.
        assert_eq!(merge(&shown, &[3], &[], false), Merge::Whole);
        // Not listed, but it belongs in the active list.
        assert_eq!(merge(&shown, &[9], &[t(9, "50")], false), Merge::Whole);
        // Listed, but archived meanwhile.
        let archived = ChatThread {
            archived_at: Some("2026-10-08 10:01:00".into()),
            ..t(3, "20")
        };
        assert_eq!(merge(&shown, &[3], &[archived], false), Merge::Whole);
    }

    #[test]
    fn a_row_that_belongs_in_the_other_list_changes_nothing() {
        let archived = |id: i64, at: &str| ChatThread {
            archived_at: Some(format!("2026-10-01 09:00:{at}")),
            ..t(id, "00")
        };
        let shown = vec![archived(5, "20"), archived(6, "10")];
        // An active thread's message, while the archived list shows.
        assert_eq!(merge(&shown, &[2], &[t(2, "40")], true), Merge::Same);
        // An archived thread's message: its row taken, the archived order
        // kept (by when it was archived, not by its time).
        let touched = ChatThread {
            updated_at: "2026-10-08 11:00:00".into(),
            ..archived(6, "10")
        };
        let Merge::List(next) = merge(&shown, &[6], std::slice::from_ref(&touched), true) else {
            panic!("the row changes");
        };
        assert_eq!(ids(&next), vec![5, 6]);
        assert_eq!(next[1], touched);
    }
}
