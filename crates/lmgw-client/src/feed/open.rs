//! The calls waiting for a decision as a client follows them (device link
//! design §7.3): the open set a [`super::FeedReader`] keeps, and the
//! changes it yields ([`ApprovalsChange`]).
//!
//! The set starts as `hello.open_approvals`, the open set at the head the
//! stream opened at, so the `approval.*` events of the catch-up are already
//! in it and change nothing. At `caught_up` the reader yields the whole set
//! ([`ApprovalsChange::Open`]); after it each `approval.requested` adds
//! ([`ApprovalsChange::Added`]) and each `approval.decided`, and the
//! deletion of the call's thread or reply, withdraws
//! ([`ApprovalsChange::Withdrawn`]), each right after the event that made
//! it. So a catch-up never raises a notification only to withdraw it two
//! events later.
//!
//! **Every `state` carries the set again** (`LiveState::open_approvals`;
//! the owner's ruling of 2026-10-15): lmgw sends one after a move of what a
//! device may see — a thread brought into its reach still holds the calls
//! that wait in it, and no `approval.requested` says them again — and after
//! a `resync` that passed records. The reader replaces its set with it:
//! before `caught_up` silently, after it yielding `Withdrawn` for what the
//! set lost and `Added` for what it gained, by id, so a set read a little
//! past the stream's cursor is safe. A `state` whose list is `null` (lmgw
//! could not read it) leaves the set as it is.
//!
//! **Across reconnects.** A reader lives for one connection, so `Open`
//! comes again at every reconnect with every call that waits, and a call
//! decided while the link was down is only missing from it: its
//! `approval.decided` came before `caught_up`, which the set already
//! reflects. `Open` is therefore the whole set to reconcile against: raise
//! what is not shown, withdraw what is shown but absent, and remember the
//! ids the user dismissed so as not to raise them again. A reader made
//! with [`super::FeedReader::knowing`] does that comparison itself, against
//! the ids the client shows or dismissed, and yields `Withdrawn` and
//! `Added` instead of `Open`.

use std::collections::BTreeSet;

use super::{ApprovalRequested, FeedEvent, ThreadNow};

/// A change of the calls waiting for a decision (device link design §7.3),
/// as a client that notifies acts on it (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalsChange {
    /// `caught_up` came: the whole set of calls waiting now, oldest first
    /// (what `hello.open_approvals` said; the catch-up changed nothing in
    /// it). It comes again at every connection: reconcile what you show
    /// against it. Raise a notification for each call you neither show nor
    /// dismissed, withdraw each one you show that is not in it (decided,
    /// or its reply or thread gone, while the link was down), and keep the
    /// ids the user dismissed so they are not raised again. A reader made
    /// with [`super::FeedReader::knowing`] yields `Withdrawn` and `Added`
    /// instead.
    Open(Vec<ApprovalRequested>),
    /// A call that waits now and the client does not know yet: after
    /// `caught_up` (asked for, or in a `state`'s set and not in the one
    /// before), or at it for a reader that knows what the client shows.
    /// Notify for it — unless the user dismissed it: a call that left the
    /// reader's reach and came back within one connection, or one
    /// dismissed after [`super::FeedReader::knowing`] was told what is
    /// known, arrives as `Added` again, so filter by your dismissed ids
    /// here too, as for `Open`.
    Added(ApprovalRequested),
    /// Calls that wait no more, by `approval_request_id`: decided, their
    /// reply or thread gone, or missing from a `state`'s set (out of the
    /// reader's reach now) — after `caught_up`, or at it the known ones
    /// missing from the set. Withdraw their notifications.
    Withdrawn(Vec<String>),
}

/// One connection's open set (module doc).
#[derive(Debug, Clone, Default)]
pub(super) struct OpenSet {
    /// The calls waiting for a decision, oldest first.
    pub open: Vec<ApprovalRequested>,
    /// `caught_up` came on this connection.
    pub caught_up: bool,
    /// The approval ids the client showed or dismissed before this
    /// connection ([`super::FeedReader::knowing`]): `caught_up` yields the
    /// difference to them instead of the whole set.
    pub known: Option<BTreeSet<String>>,
}

impl OpenSet {
    /// What `ev` does to the set, in the order a client applies it.
    pub fn apply(&mut self, ev: &FeedEvent) -> Vec<ApprovalsChange> {
        match ev {
            FeedEvent::Hello(h) => {
                self.open = h.open_approvals.clone();
                self.caught_up = false;
                Vec::new()
            }
            FeedEvent::CaughtUp(_) => {
                self.caught_up = true;
                self.at_caught_up()
            }
            // The set again, as lmgw read it when it sent the `state`.
            FeedEvent::State(st) => {
                let Some(set) = &st.open_approvals else {
                    return Vec::new();
                };
                let before = ids(&self.open);
                self.open = set.clone();
                if self.caught_up {
                    diff(&before, &self.open)
                } else {
                    Vec::new()
                }
            }
            // In the catch-up: already in the set `hello` gave.
            _ if !self.caught_up => Vec::new(),
            FeedEvent::ApprovalRequested(a) => {
                let id = &a.approval_request_id;
                match self.open.iter_mut().find(|o| &o.approval_request_id == id) {
                    // The same call again: kept as it is now, no change.
                    Some(o) => {
                        *o = a.clone();
                        Vec::new()
                    }
                    None => {
                        self.open.push(a.clone());
                        vec![ApprovalsChange::Added(a.clone())]
                    }
                }
            }
            FeedEvent::ApprovalDecided(d) => {
                self.withdraw(|a| a.approval_request_id == d.approval_request_id)
            }
            FeedEvent::ThreadDeleted(g)
            | FeedEvent::ThreadCreated(ThreadNow::Gone(g))
            | FeedEvent::ThreadUpdated(ThreadNow::Gone(g)) => {
                self.withdraw(|a| a.thread_id == g.thread_id)
            }
            FeedEvent::MessageDeleted(m) => self
                .withdraw(|a| a.thread_id == m.thread_id && m.message_ids.contains(&a.message_id)),
            _ => Vec::new(),
        }
    }

    /// At `caught_up`: the whole set, or, for a reader that knows what the
    /// client shows, what it shows and the set lacks, then each call of the
    /// set it does not know, oldest first.
    fn at_caught_up(&self) -> Vec<ApprovalsChange> {
        match &self.known {
            Some(known) => diff(known, &self.open),
            None => vec![ApprovalsChange::Open(self.open.clone())],
        }
    }

    /// Take the open calls `went` names out of the set: their ids, when
    /// there were any.
    fn withdraw(&mut self, went: impl Fn(&ApprovalRequested) -> bool) -> Vec<ApprovalsChange> {
        let ids: Vec<String> = self
            .open
            .iter()
            .filter(|a| went(a))
            .map(|a| a.approval_request_id.clone())
            .collect();
        self.open.retain(|a| !went(a));
        if ids.is_empty() {
            Vec::new()
        } else {
            vec![ApprovalsChange::Withdrawn(ids)]
        }
    }
}

/// The approval ids of `set`.
fn ids(set: &[ApprovalRequested]) -> BTreeSet<String> {
    set.iter().map(|a| a.approval_request_id.clone()).collect()
}

/// From the ids `shown` to `set`: `Withdrawn` with the shown ids the set
/// lacks, then `Added` for each call of the set not shown, oldest first.
fn diff(shown: &BTreeSet<String>, set: &[ApprovalRequested]) -> Vec<ApprovalsChange> {
    let now = ids(set);
    let gone: Vec<String> = shown.difference(&now).cloned().collect();
    let mut out = Vec::new();
    if !gone.is_empty() {
        out.push(ApprovalsChange::Withdrawn(gone));
    }
    out.extend(
        set.iter()
            .filter(|a| !shown.contains(&a.approval_request_id))
            .cloned()
            .map(ApprovalsChange::Added),
    );
    out
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::chat_feed::{
        ApprovalDecided, CaughtUp, Hello, MessagesDeleted, ThreadGone,
    };

    use super::super::{FeedItem, FeedReader};
    use super::*;

    /// One `approval.requested` as the feed renders it.
    fn asked(id: &str, thread_id: i64, message_id: i64) -> ApprovalRequested {
        ApprovalRequested {
            thread_id,
            thread_title: format!("thread {thread_id}"),
            message_id,
            approval_request_id: id.into(),
            server_label: "desktop".into(),
            name: "open_file".into(),
            arguments: "{}".into(),
            call_id: Some(format!("call_{id}")),
        }
    }

    /// The decision on call `id` of reply `message_id` in thread
    /// `thread_id`.
    fn decided(id: &str, thread_id: i64, message_id: i64) -> ApprovalDecided {
        ApprovalDecided {
            thread_id,
            message_id,
            approval_request_id: id.into(),
            approve: false,
            by: Some("the dashboard".into()),
            not_run: false,
            call_id: None,
        }
    }

    /// An SSE record of `event` with `data`, stored (`id`) or live.
    fn record(id: Option<&str>, event: &str, data: &impl serde::Serialize) -> String {
        let id = id.map(|i| format!("id: {i}\n")).unwrap_or_default();
        format!(
            "{id}event: {event}\ndata: {}\n\n",
            serde_json::to_string(data).unwrap()
        )
    }

    /// `hello` with `open` waiting, and the stream's cursor `cursor`.
    fn hello(cursor: &str, open: &[&ApprovalRequested]) -> String {
        record(
            None,
            "hello",
            &Hello {
                epoch: "e".into(),
                cursor: cursor.into(),
                keepalive_s: 120,
                open_approvals: open.iter().map(|a| (*a).clone()).collect(),
                ..Hello::default()
            },
        )
    }

    fn caught_up(cursor: &str) -> String {
        record(
            None,
            "caught_up",
            &CaughtUp {
                cursor: cursor.into(),
            },
        )
    }

    /// The open-set changes among `items`.
    fn changes(items: Vec<FeedItem>) -> Vec<ApprovalsChange> {
        items
            .into_iter()
            .filter_map(|i| match i {
                FeedItem::Approvals(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// The set starts as `hello`'s; the catch-up's `approval.*` (a request
    /// and its decision, both before the head) change nothing; `caught_up`
    /// yields the whole set and moves the cursor; after it a request adds,
    /// a decision, its reply's deletion and its thread's withdraw, each
    /// right after its event.
    #[test]
    fn the_open_set_starts_at_hello_and_moves_only_after_caught_up() {
        let a = asked("mcpr_a", 7, 41);
        let b = asked("mcpr_b", 8, 50);
        let mut r = FeedReader::new(Some("e:3:aa"));
        let got = r.push(
            [
                hello("e:5:cc", &[&a]),
                record(Some("e:4:bb"), "approval.requested", &b),
                record(
                    Some("e:5:cc"),
                    "approval.decided",
                    &decided("mcpr_b", 8, 50),
                ),
                caught_up("e:5:cc"),
            ]
            .concat()
            .as_bytes(),
        );
        assert!(matches!(
            &got[got.len() - 2],
            FeedItem::Event(FeedEvent::CaughtUp(_))
        ));
        assert_eq!(
            changes(got),
            vec![ApprovalsChange::Open(vec![a.clone()])],
            "the catch-up changed nothing"
        );
        assert!(r.caught_up());
        assert_eq!(r.open_approvals(), std::slice::from_ref(&a));
        assert_eq!(r.cursor(), Some("e:5:cc"));

        let c = asked("mcpr_c", 9, 60);
        let d = asked("mcpr_d", 10, 70);
        let got = r.push(
            [
                record(Some("e:6:dd"), "approval.requested", &c),
                record(Some("e:7:ee"), "approval.requested", &c),
                record(Some("e:8:ff"), "approval.requested", &d),
                record(
                    Some("e:9:01"),
                    "approval.decided",
                    &decided("mcpr_a", 7, 41),
                ),
                record(
                    Some("e:10:02"),
                    "message.deleted",
                    &MessagesDeleted {
                        thread_id: 9,
                        message_ids: vec![60],
                        by: None,
                    },
                ),
                record(
                    Some("e:11:03"),
                    "thread.deleted",
                    &ThreadGone {
                        thread_id: 10,
                        deleted: true,
                        by: None,
                    },
                ),
            ]
            .concat()
            .as_bytes(),
        );
        assert_eq!(
            changes(got),
            vec![
                ApprovalsChange::Added(c.clone()),
                ApprovalsChange::Added(d.clone()),
                ApprovalsChange::Withdrawn(vec!["mcpr_a".into()]),
                ApprovalsChange::Withdrawn(vec!["mcpr_c".into()]),
                ApprovalsChange::Withdrawn(vec!["mcpr_d".into()]),
            ]
        );
        assert!(r.open_approvals().is_empty());
    }

    /// A new `hello` starts the set over, and the catch-up waits for its
    /// own `caught_up` again.
    #[test]
    fn a_new_hello_starts_the_set_over() {
        let a = asked("mcpr_a", 7, 41);
        let mut r = FeedReader::new(None);
        r.push([hello("e:0", &[]), caught_up("e:0")].concat().as_bytes());
        assert!(r.caught_up());
        let got = r.push(
            [
                hello("e:1:aa", &[&a]),
                record(
                    Some("e:1:aa"),
                    "approval.decided",
                    &decided("mcpr_a", 7, 41),
                ),
            ]
            .concat()
            .as_bytes(),
        );
        assert!(!r.caught_up());
        assert!(changes(got).is_empty(), "before caught_up nothing moves");
        assert_eq!(r.open_approvals(), &[a]);
    }

    /// A `state` with the open set (the owner's ruling of 2026-10-15):
    /// before `caught_up` it replaces the set silently, so `Open` hands over
    /// the set it said; after it, the difference by id; `null` changes
    /// nothing.
    #[test]
    fn a_state_s_set_replaces_the_set_by_id() {
        let a = asked("mcpr_a", 7, 41);
        let b = asked("mcpr_b", 8, 50);
        let c = asked("mcpr_c", 9, 60);
        let state = |open: Option<Vec<&ApprovalRequested>>| {
            record(
                None,
                "state",
                &lmgw_api_types::chat_feed::LiveState {
                    open_approvals: open.map(|o| o.into_iter().cloned().collect()),
                    ..Default::default()
                },
            )
        };
        let mut r = FeedReader::new(None);
        let got = r.push(
            [
                hello("e:5:aa", &[&a]),
                state(Some(vec![&a, &b])),
                caught_up("e:6:bb"),
            ]
            .concat()
            .as_bytes(),
        );
        assert_eq!(
            changes(got),
            vec![ApprovalsChange::Open(vec![a.clone(), b.clone()])]
        );
        let got = r.push([state(None), state(Some(vec![&b, &c]))].concat().as_bytes());
        assert_eq!(
            changes(got),
            vec![
                ApprovalsChange::Withdrawn(vec!["mcpr_a".into()]),
                ApprovalsChange::Added(c.clone()),
            ]
        );
        assert_eq!(r.open_approvals(), &[b.clone(), c.clone()]);
        // An event that follows and says what the set says changes nothing.
        let got = r.push(record(Some("e:7:cc"), "approval.requested", &c).as_bytes());
        assert!(changes(got).is_empty());
        // A reader that knows what the client shows reconciles against the
        // `state`'s set at `caught_up`.
        let mut knowing = FeedReader::new(None).knowing(["mcpr_a".to_string()]);
        let got = knowing.push(
            [
                hello("e:5:aa", &[&a]),
                state(Some(vec![&b])),
                caught_up("e:6:bb"),
            ]
            .concat()
            .as_bytes(),
        );
        assert_eq!(
            changes(got),
            vec![
                ApprovalsChange::Withdrawn(vec!["mcpr_a".into()]),
                ApprovalsChange::Added(b),
            ]
        );
    }

    /// Review S1: across a reconnect. The first connection's client shows
    /// `a` and `b`; while the link is down `a` is decided and `c` is asked
    /// for. The next connection's `Open` lacks `a` and holds `c`; a reader
    /// that knows what the client shows withdraws `a` and adds `c` instead,
    /// and raises nothing it knows (`b`, and `d`, which the user
    /// dismissed).
    #[test]
    fn a_reader_that_knows_what_is_shown_reconciles_at_caught_up() {
        let a = asked("mcpr_a", 7, 41);
        let b = asked("mcpr_b", 8, 50);
        let c = asked("mcpr_c", 9, 60);
        let d = asked("mcpr_d", 10, 70);
        let mut first = FeedReader::new(None);
        let got = first.push(
            [hello("e:5:aa", &[&a, &b, &d]), caught_up("e:5:aa")]
                .concat()
                .as_bytes(),
        );
        assert_eq!(
            changes(got),
            vec![ApprovalsChange::Open(vec![a.clone(), b.clone(), d.clone()])]
        );
        // The user swiped `d` away; the client shows `a` and `b`.
        let known = ["mcpr_a", "mcpr_b", "mcpr_d"].map(String::from);

        // The link drops; `a` is decided and `c` asked for meanwhile.
        let next = || {
            [
                hello("e:8:dd", &[&b, &c, &d]),
                record(
                    Some("e:6:bb"),
                    "approval.decided",
                    &decided("mcpr_a", 7, 41),
                ),
                record(Some("e:7:cc"), "approval.requested", &c),
                caught_up("e:8:dd"),
            ]
            .concat()
        };
        let mut plain = FeedReader::new(first.cursor());
        assert_eq!(
            changes(plain.push(next().as_bytes())),
            vec![ApprovalsChange::Open(vec![b.clone(), c.clone(), d.clone()])],
            "the whole set: a client reconciles it itself"
        );
        let mut knowing = FeedReader::new(first.cursor()).knowing(known);
        assert_eq!(
            changes(knowing.push(next().as_bytes())),
            vec![
                ApprovalsChange::Withdrawn(vec!["mcpr_a".into()]),
                ApprovalsChange::Added(c.clone()),
            ]
        );
        assert_eq!(knowing.open_approvals(), &[b, c, d]);
        // After `caught_up` it goes on as any reader does.
        let got = knowing.push(
            record(
                Some("e:9:ee"),
                "approval.decided",
                &decided("mcpr_c", 9, 60),
            )
            .as_bytes(),
        );
        assert_eq!(
            changes(got),
            vec![ApprovalsChange::Withdrawn(vec!["mcpr_c".into()])]
        );
    }
}
