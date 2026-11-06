//! What one event of the Chat's own feed says to the page (chat-plane design
//! D1), on plain values: the threads, folders and threads' messages it
//! names — what the follower reads again — and the GPU hold the voice
//! controls show.
//!
//! The names are the ones the shell stream's `chat` frame names: a
//! thread's row (created, changed, deleted, a task started or ended in it),
//! a folder (created, changed, deleted, its current thread moved), a
//! thread's messages. `hello` and `resync` say "read everything again", as
//! a (re)connect always has. An event the page has no use for (a turn, a
//! voice session, an approval, a profile, a transfer) says nothing.

use lmgw_api_types::chat_feed::{FeedEvent, FeedHold, FolderNow, ThreadNow};
use lmgw_api_types::ChatChanged;

/// What an event says (module doc).
#[derive(Debug, Default, PartialEq)]
pub(super) struct Said {
    pub changed: Option<ChatChanged>,
    pub hold: Option<FeedHold>,
}

fn changed(c: ChatChanged) -> Said {
    Said {
        changed: Some(c),
        hold: None,
    }
}

fn thread(id: i64) -> Said {
    changed(ChatChanged {
        threads: vec![id],
        ..Default::default()
    })
}

fn folder(id: i64) -> Said {
    changed(ChatChanged {
        folders: vec![id],
        ..Default::default()
    })
}

fn messages(id: i64) -> Said {
    changed(ChatChanged {
        messages: vec![id],
        ..Default::default()
    })
}

fn hold(h: FeedHold) -> Said {
    Said {
        changed: None,
        hold: Some(h),
    }
}

/// What `ev` says (module doc).
pub(super) fn said(ev: FeedEvent) -> Said {
    use FeedEvent as E;
    match ev {
        E::Hello(h) => Said {
            changed: Some(ChatChanged::resync()),
            hold: Some(h.hold),
        },
        E::Resync(_) => changed(ChatChanged::resync()),
        E::ThreadCreated(t) | E::ThreadUpdated(t) => thread(match t {
            ThreadNow::Row(r) => r.thread.id,
            ThreadNow::Gone(g) => g.thread_id,
        }),
        E::ThreadDeleted(g) => thread(g.thread_id),
        E::FolderCreated(f) | E::FolderUpdated(f) => folder(match f {
            FolderNow::Row(r) => r.folder.id,
            FolderNow::Gone(g) => g.folder_id,
        }),
        E::FolderDeleted(g) => folder(g.folder_id),
        E::FolderCurrent(c) => folder(c.folder_id),
        E::MessageAdded(m) | E::MessageUpdated(m) => messages(m.thread_id),
        E::MessageDeleted(m) => messages(m.thread_id),
        // A task's start changes the thread's task list, its end adds a
        // message (MCP Tasks design §4.1).
        E::TaskStarted(t) => thread(t.thread_id),
        E::TaskDone(t) => thread(t.thread_id),
        E::Hold(h) => hold(h),
        E::State(s) => hold(s.hold),
        _ => Said::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(name: &str, data: &str) -> Said {
        said(FeedEvent::parse(name, data).unwrap())
    }

    fn names(threads: &[i64], folders: &[i64], messages: &[i64]) -> Option<ChatChanged> {
        Some(ChatChanged {
            threads: threads.to_vec(),
            folders: folders.to_vec(),
            messages: messages.to_vec(),
            resync: false,
        })
    }

    #[test]
    fn rows_and_tombstones_name_their_thread_or_folder() {
        assert_eq!(
            parse("thread.created", r#"{"id":7,"title":"t","by":null}"#).changed,
            names(&[7], &[], &[])
        );
        assert_eq!(
            parse(
                "thread.updated",
                r#"{"thread_id":7,"deleted":true,"by":null}"#
            )
            .changed,
            names(&[7], &[], &[])
        );
        assert_eq!(
            parse("thread.deleted", r#"{"thread_id":9,"deleted":true}"#).changed,
            names(&[9], &[], &[])
        );
        assert_eq!(
            parse("folder.updated", r#"{"id":3,"name":"f","by":null}"#).changed,
            names(&[], &[3], &[])
        );
        assert_eq!(
            parse("folder.deleted", r#"{"folder_id":4,"deleted":true}"#).changed,
            names(&[], &[4], &[])
        );
        assert_eq!(
            parse(
                "folder.current",
                r#"{"folder_id":5,"thread_id":8,"previous_thread_id":null,"reason":"idle"}"#
            )
            .changed,
            names(&[], &[5], &[])
        );
    }

    #[test]
    fn messages_and_tasks_name_their_thread() {
        assert_eq!(
            parse(
                "message.added",
                r#"{"thread_id":7,"thread_title":"t","messages":[]}"#
            )
            .changed,
            names(&[], &[], &[7])
        );
        assert_eq!(
            parse("message.deleted", r#"{"thread_id":7,"message_ids":[1]}"#).changed,
            names(&[], &[], &[7])
        );
        // The task's own id is not the thread's.
        assert_eq!(
            parse(
                "task.started",
                r#"{"thread_id":7,"id":99,"task_id":"x","server_label":"s","tool":"t"}"#
            )
            .changed,
            names(&[7], &[], &[])
        );
    }

    #[test]
    fn hello_reads_everything_and_says_the_hold() {
        let s = parse(
            "hello",
            r#"{"epoch":"e","cursor":"e:1:t","keepalive_s":30,"hold":{"active":true,"benchmark":false}}"#,
        );
        assert_eq!(s.changed, Some(ChatChanged::resync()));
        assert_eq!(
            s.hold,
            Some(FeedHold {
                active: true,
                benchmark: false
            })
        );
        assert_eq!(
            parse("resync", r#"{"reason":"r"}"#).changed,
            Some(ChatChanged::resync())
        );
        let h = parse("hold", r#"{"active":false,"benchmark":true}"#);
        assert_eq!((h.changed, h.hold.map(|h| h.benchmark)), (None, Some(true)));
        assert!(parse("state", r#"{"hold":{"active":true}}"#)
            .hold
            .is_some_and(|h| h.active));
    }

    #[test]
    fn what_the_page_has_no_use_for_says_nothing() {
        assert_eq!(
            parse("turn.started", r#"{"thread_id":7,"by":"device 'phone'"}"#),
            Said::default()
        );
        assert_eq!(parse("caught_up", r#"{"cursor":"e:1:t"}"#), Said::default());
    }
}
