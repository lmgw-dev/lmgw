//! The thread read back when a voice session ends (chat-voice §9.5; WP9
//! review m5, WP11 UI review m1, m2).
//!
//! The page streams a voice turn's bubbles from the session's events; what
//! the journal stored — a reply cut to what was heard, the last turn written
//! as the session drained — is read back from the gateway, so the thread
//! shows exactly that:
//! - **the page closed the socket** (leaving: Esc, Leave, another thread;
//!   and an end of the page's own: the microphone ended, the page hidden):
//!   once, when the browser says the socket closed. The gateway answers the
//!   page's close only once its journal drained, mid-reply too (a close
//!   frame, never a reset: `socket.rs`), so that one read holds every
//!   write. The session count is taken when the session ends, not when the
//!   close comes, so a session entered meanwhile on the same thread is never
//!   replaced by its predecessor's read (review m1).
//!   **A close that never comes** (the network went): after
//!   [`close_wait_ms`] — the gateway's own bound on its drain and
//!   [`CLOSE_SLACK_MS`] — the thread is read anyway, Keep stops waiting, and
//!   the line says so; a close that still comes reads again;
//! - **the gateway closed it** (a takeover, the network, the gateway's own
//!   end): no close of the page's own marks the drain, so the thread is read
//!   at once and again every [`SETTLE_MS`] until two reads agree, and once
//!   more after [`close_wait_ms`] — a last transcript made on the CPU can
//!   take longer than one settle (review NIT 4).
//!
//! A read lands only while nothing newer owns the transcript: the same
//! thread on screen, no reply of this thread streaming as text, and no
//! voice session entered since.
//!
//! **In place** (review m2): the stored rows are matched to the page's
//! messages by their ids ([`plan`]). A matched message takes the row's text,
//! voice, reasoning, tokens and model where they differ; a bubble the store
//! lacks (a refused turn's, one the journal removed) goes; rows past the
//! last one shown are added. Only a list whose order differs is loaded
//! afresh. So a read that changes nothing re-mounts nothing — an open
//! `<details>` stays open, the scroll stays where it is.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use serde_json::Value;

use super::super::super::chat::{Msg, MsgRow, ThreadDetail};
use super::super::audio::player::sleep;
use super::super::state::{Note, NoteKind};
use super::Realtime;

/// How long a session the gateway ended waits between two reads.
const SETTLE_MS: u64 = 1_000;

/// Beyond the gateway's own bound on its drain: the network, the journal's
/// last writes after the transcripts.
const CLOSE_SLACK_MS: u64 = 10_000;

/// The wait for a close with `realtime.ping_interval_s` 0 (the owner's "no
/// bound": the gateway may take as long as its calls do) or not read. Not a
/// cap: past it the thread is read and the line says so, and the close
/// that still comes reads again.
const CLOSE_WAIT_UNBOUNDED_MS: u64 = 60_000;

/// The note a close that did not come leaves.
const LATE_CLOSE_KEY: &str = "realtime-close";

/// How long the page waits for the gateway's close before it reads the
/// thread anyway: the gateway bounds its end by two
/// `realtime.ping_interval_s` (the last transcripts, then the writer's last
/// events, §8.6), plus [`CLOSE_SLACK_MS`].
pub(super) fn close_wait_ms(ping_interval_s: Option<u32>) -> u64 {
    match ping_interval_s {
        Some(p) if p > 0 => 2 * u64::from(p) * 1000 + CLOSE_SLACK_MS,
        _ => CLOSE_WAIT_UNBOUNDED_MS,
    }
}

/// A session that ended: its thread, the session count it had, and how long
/// its gateway may take to close.
#[derive(Clone, Copy)]
pub(super) struct Ended {
    pub tid: i64,
    pub session: u64,
    pub wait_ms: u64,
}

/// The page closed the socket (module doc): the callback the socket's close
/// calls, and the fallback for a close that never comes. Keep waits until
/// either (`Realtime::closing`).
pub(super) fn at_close(rt: Realtime, e: Ended) -> Box<dyn FnOnce()> {
    rt.pending_closes.update_value(|n| *n += 1);
    rt.closing.try_set(true);
    let settled = Rc::new(Cell::new(false));
    let late = settled.clone();
    let timer = set_timeout_with_handle(
        move || {
            if late.replace(true) {
                return;
            }
            let_go(rt);
            let line = if rt.pv.voice_mode.try_get_untracked() == Some(true) {
                rt.status
            } else {
                rt.pv.status
            };
            line.set(Note::new(
                LATE_CLOSE_KEY,
                NoteKind::Warn,
                format!(
                    "the gateway has not closed the voice session after {} s (the network?): \
                     the conversation shows what was stored so far, and is read again if the \
                     session still closes",
                    e.wait_ms / 1000
                ),
            ));
            read(rt, e.tid, e.session, Until::Once);
        },
        Duration::from_millis(e.wait_ms),
    )
    .ok();
    Box::new(move || {
        if let Some(t) = timer {
            t.clear();
        }
        if settled.replace(true) {
            rt.status.clear(LATE_CLOSE_KEY);
            rt.pv.status.clear(LATE_CLOSE_KEY);
        } else {
            let_go(rt);
        }
        read(rt, e.tid, e.session, Until::Once);
    })
}

/// One session less is closing; Keep waits for none.
fn let_go(rt: Realtime) {
    let left = rt
        .pending_closes
        .try_update_value(|n| {
            *n = n.saturating_sub(1);
            *n
        })
        .unwrap_or(0);
    if left == 0 {
        rt.closing.try_set(false);
    }
}

/// How often the thread is read.
#[derive(Clone, Copy)]
pub(super) enum Until {
    Once,
    /// Until two reads agree, then once more after `last_after_ms`.
    Settled {
        last_after_ms: u64,
    },
}

/// Read thread `tid` back for session `session` (module doc).
pub(super) fn read(rt: Realtime, tid: i64, session: u64, until: Until) {
    rt.parts.scope.spawn(async move {
        let mut last: Option<Value> = None;
        loop {
            let Some((raw, rows)) = fetch(rt, tid, session).await else {
                return;
            };
            if last.as_ref() == Some(&raw) {
                break;
            }
            apply(rt, rows);
            match until {
                Until::Once => return,
                Until::Settled { .. } => {
                    last = Some(raw);
                    sleep(SETTLE_MS).await;
                }
            }
        }
        if let Until::Settled { last_after_ms } = until {
            sleep(last_after_ms).await;
            if let Some((raw, rows)) = fetch(rt, tid, session).await {
                if last.as_ref() != Some(&raw) {
                    apply(rt, rows);
                }
            }
        }
    });
}

/// The thread's messages, raw (to compare reads) and typed; `None` when the
/// read failed or something newer owns the transcript now.
async fn fetch(rt: Realtime, tid: i64, session: u64) -> Option<(Value, Vec<MsgRow>)> {
    let v = crate::api::get::<Value>(format!("/chat/api/threads/{tid}"))
        .await
        .ok()?;
    if !owns(rt, tid, session) {
        return None;
    }
    let raw = v["messages"].clone();
    let d = serde_json::from_value::<ThreadDetail>(v).ok()?;
    Some((raw, d.messages))
}

/// Nothing newer owns the transcript (module doc). A text reply streaming
/// in another thread is no business of this one's (review NIT 3).
fn owns(rt: Realtime, tid: i64, session: u64) -> bool {
    rt.sessions.try_get_value() == Some(session)
        && rt
            .parts
            .current
            .try_with_untracked(|c| c.as_ref().map(|t| t.id))
            == Some(Some(tid))
        && matches!(rt.parts.streaming.try_get_untracked(), Some(s) if s != Some(tid))
}

/// How the page's messages become the stored rows.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// In place: `pairs` (page index, row index) take the row's fields,
    /// `drop` (page indices) go, `append` (row indices) are added at the end.
    Patch {
        pairs: Vec<(usize, usize)>,
        drop: Vec<usize>,
        append: Vec<usize>,
    },
    /// The order differs: load the rows afresh.
    Reload,
}

/// Match the page's messages (`(stored id, role)`) to the rows (`(id,
/// role)`), both in order (module doc).
pub(super) fn plan(page: &[(Option<i64>, &str)], rows: &[(i64, &str)]) -> Plan {
    let mut pairs = Vec::new();
    let mut drop = Vec::new();
    let mut next = 0usize;
    for (pi, (id, role)) in page.iter().enumerate() {
        let Some(ri) = id.and_then(|id| rows.iter().position(|(r, _)| *r == id)) else {
            // No row: a bubble the store lacks.
            drop.push(pi);
            continue;
        };
        if ri < next || rows[ri].1 != *role {
            return Plan::Reload;
        }
        // A row between two shown ones that the page lacks: not an append.
        if ri > next && !pairs.is_empty() {
            return Plan::Reload;
        }
        if pairs.is_empty() && ri > 0 {
            return Plan::Reload;
        }
        pairs.push((pi, ri));
        next = ri + 1;
    }
    if pairs.is_empty() && !rows.is_empty() && !page.is_empty() {
        // Nothing shown is stored: what is, is the thread afresh.
        return Plan::Reload;
    }
    Plan::Patch {
        pairs,
        drop,
        append: (next..rows.len()).collect(),
    }
}

/// Show `rows` as the open thread's messages (module doc).
fn apply(rt: Realtime, rows: Vec<MsgRow>) {
    let msgs = rt.parts.msgs;
    let Some(page) = msgs.try_with_untracked(|v| {
        v.iter()
            .map(|m| (m.db_id.get_untracked(), m.role.clone()))
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    let page_keys: Vec<(Option<i64>, &str)> =
        page.iter().map(|(id, r)| (*id, r.as_str())).collect();
    let row_keys: Vec<(i64, &str)> = rows.iter().map(|r| (r.id, r.role.as_str())).collect();
    let (pairs, drop, append) = match plan(&page_keys, &row_keys) {
        Plan::Reload => return rt.parts.load.run(rows),
        Plan::Patch {
            pairs,
            drop,
            append,
        } => (pairs, drop, append),
    };
    msgs.with_untracked(|v| {
        for &(pi, ri) in &pairs {
            patch(&v[pi], &rows[ri]);
        }
    });
    if drop.is_empty() && append.is_empty() {
        return;
    }
    let mut added = rt
        .parts
        .make
        .run(append.iter().map(|&ri| rows[ri].clone()).collect());
    // What a user turn retrieved belongs to the answer after it, which the
    // page made without the turn when the turn was shown already.
    for (m, &ri) in added.iter_mut().zip(&append) {
        if m.role == "assistant" && m.context.with_untracked(Option::is_none) {
            if let Some(c) = rows[..ri]
                .iter()
                .rev()
                .find(|r| r.role == "user")
                .and_then(|r| r.context.clone())
            {
                m.context.set(Some(c));
            }
        }
    }
    msgs.update(|v| {
        let mut i = 0usize;
        v.retain(|_| {
            let keep = !drop.contains(&i);
            i += 1;
            keep
        });
        v.extend(added);
    });
}

/// Message `m` takes stored row `r`'s fields where they differ.
fn patch(m: &Msg, r: &MsgRow) {
    if m.content.with_untracked(|c| *c != r.content) {
        m.content.set(r.content.clone());
    }
    if m.reasoning.with_untracked(|c| *c != r.reasoning) {
        m.reasoning.set(r.reasoning.clone());
    }
    if m.voice.with_untracked(|v| *v != r.voice) {
        m.voice.set(r.voice.clone());
    }
    let tokens = r.prompt_tokens.zip(r.completion_tokens);
    if tokens.is_some() && m.tokens.get_untracked() != tokens {
        m.tokens.set(tokens);
    }
    if r.model.is_some() && m.model.with_untracked(|x| *x != r.model) {
        m.model.set(r.model.clone());
        m.answered_by.set(r.answered_by.clone());
    }
    if m.streaming.get_untracked() {
        m.streaming.set(false);
    }
    if m.unsaved.get_untracked() {
        m.unsaved.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_of(pairs: &[(usize, usize)], drop: &[usize], append: &[usize]) -> Plan {
        Plan::Patch {
            pairs: pairs.to_vec(),
            drop: drop.to_vec(),
            append: append.to_vec(),
        }
    }

    #[test]
    fn a_read_that_matches_changes_nothing_in_the_list() {
        let page = [(Some(1), "user"), (Some(2), "assistant")];
        let rows = [(1, "user"), (2, "assistant")];
        assert_eq!(plan(&page, &rows), patch_of(&[(0, 0), (1, 1)], &[], &[]));
        assert_eq!(plan(&[], &[]), patch_of(&[], &[], &[]));
    }

    #[test]
    fn a_late_turn_is_appended_and_a_bubble_the_store_lacks_goes() {
        // The last user turn was transcribed during the drain: the page never
        // heard of it.
        let page = [(Some(1), "user"), (Some(2), "assistant")];
        let rows = [(1, "user"), (2, "assistant"), (3, "user")];
        assert_eq!(plan(&page, &rows), patch_of(&[(0, 0), (1, 1)], &[], &[2]));
        // A refused turn's reply bubble (no id) and one the journal removed.
        let page = [
            (Some(1), "user"),
            (None, "assistant"),
            (Some(3), "user"),
            (Some(4), "assistant"),
        ];
        let rows = [(1, "user"), (3, "user")];
        assert_eq!(
            plan(&page, &rows),
            patch_of(&[(0, 0), (2, 1)], &[1, 3], &[])
        );
        // An empty page (a fresh temporary chat) takes every row.
        assert_eq!(
            plan(&[], &[(1, "user"), (2, "assistant")]),
            patch_of(&[], &[], &[0, 1])
        );
    }

    #[test]
    fn another_order_loads_afresh() {
        let rows = [(1, "user"), (2, "assistant"), (3, "user")];
        // A row between two shown ones.
        assert_eq!(
            plan(&[(Some(1), "user"), (Some(3), "user")], &rows),
            Plan::Reload
        );
        // Rows before the first one shown.
        assert_eq!(plan(&[(Some(2), "assistant")], &rows), Plan::Reload);
        // Swapped.
        assert_eq!(
            plan(&[(Some(2), "assistant"), (Some(1), "user")], &rows),
            Plan::Reload
        );
        // A role that differs.
        assert_eq!(plan(&[(Some(1), "assistant")], &rows), Plan::Reload);
        // Nothing shown is stored.
        assert_eq!(plan(&[(None, "user")], &rows), Plan::Reload);
    }

    #[test]
    fn the_close_waits_for_the_gateways_own_bound() {
        assert_eq!(close_wait_ms(Some(20)), 50_000);
        assert_eq!(close_wait_ms(Some(0)), CLOSE_WAIT_UNBOUNDED_MS);
        assert_eq!(close_wait_ms(None), CLOSE_WAIT_UNBOUNDED_MS);
    }
}
