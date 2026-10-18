//! The Chat page's thread types from the gateway's: the page's
//! [`ChatThread`], list answer and folders are built from what the server
//! writes (`lmgw_api_types::chat`), field by field, so a field the gateway
//! adds has to be decided here (each conversion names every field of the
//! type it takes apart, so the build says which).
//!
//! - **A thread** is read as the gateway's [`Thread`] (create, pin, archive,
//!   the open thread of [`ThreadDetail`](super::ThreadDetail)) or its
//!   [`ThreadRow`] (the lists): the row's fields, plus what `Thread` adds
//!   (`voice_resolved`, `continue`). The page's copy has no `created_at` and
//!   no `last_message_at`, on purpose: its `PartialEq` decides whether a row
//!   read again changed anything the page shows (`chat_sync::row_take`,
//!   `chat_sync::rows::merge`), and a time that only moves with the messages
//!   must not re-seed the form.
//! - **The list** ([`ThreadsResponse`]) is the gateway's [`ThreadList`].
//! - **A folder** ([`FolderInfo`]) keeps its `defaults` as JSON: the form
//!   reads them into a [`ChatThread`] by name.

use lmgw_api_types::chat::{Folder, Thread, ThreadList, ThreadRow};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{ChatThread, ThreadsResponse};
use crate::pages::chat_folders::FolderInfo;

/// A thread of the lists: no `voice_resolved`, no `continue` (the list does
/// not resolve them).
impl From<ThreadRow> for ChatThread {
    fn from(row: ThreadRow) -> Self {
        let ThreadRow {
            id,
            title,
            model_alias,
            system_prompt,
            temperature,
            max_tokens,
            top_p,
            top_k,
            min_p,
            repeat_penalty,
            presence_penalty,
            frequency_penalty,
            seed,
            stop,
            kind,
            mcp_tools,
            reasoning_enabled,
            reasoning_effort,
            reasoning_budget,
            agent_id,
            pinned,
            archived_at,
            folder_id,
            profile_id,
            kb_ids,
            kb_mode,
            kb_budget_tokens,
            voice,
            // Not held by the page (module doc).
            created_at: _,
            updated_at,
            last_message_at: _,
            purge_at,
            temporary,
        } = row;
        Self {
            id,
            title,
            model_alias,
            system_prompt,
            temperature,
            max_tokens,
            kind,
            mcp_tools,
            reasoning_enabled,
            reasoning_effort,
            reasoning_budget,
            top_p,
            top_k,
            min_p,
            repeat_penalty,
            presence_penalty,
            frequency_penalty,
            seed,
            stop,
            kb_ids,
            kb_mode: kb_mode.as_str().to_string(),
            kb_budget_tokens,
            agent_id,
            updated_at,
            pinned,
            archived_at,
            folder_id,
            purge_at,
            temporary,
            cont: None,
            voice,
            voice_resolved: None,
            profile_id,
        }
    }
}

/// A thread read whole: its row, and what its voice resolves to and whether
/// its last reply can be continued.
impl From<Thread> for ChatThread {
    fn from(thread: Thread) -> Self {
        let Thread {
            row,
            voice_resolved,
            continue_state,
        } = thread;
        Self {
            cont: continue_state,
            voice_resolved: Some(voice_resolved),
            ..row.into()
        }
    }
}

/// A folder as the list carries it, its `defaults` as the JSON the server
/// wrote (every key, `null` where unset).
impl From<Folder> for FolderInfo {
    fn from(folder: Folder) -> Self {
        let Folder {
            id,
            name,
            sort,
            defaults,
            ongoing,
            archive_days,
            purge_days,
            devices_hidden,
            created_at: _,
            updated_at: _,
            threads_active,
            threads_archived,
        } = folder;
        Self {
            id,
            name,
            sort,
            defaults: serde_json::to_value(defaults).unwrap_or(Value::Null),
            threads_active,
            threads_archived,
            ongoing,
            archive_days,
            purge_days,
            devices_hidden,
        }
    }
}

impl From<ThreadList> for ThreadsResponse {
    fn from(list: ThreadList) -> Self {
        let ThreadList {
            threads,
            archived_count,
            temporary,
            folders,
        } = list;
        Self {
            threads: threads.into_iter().map(ChatThread::from).collect(),
            archived_count,
            temporary: temporary.into_iter().map(ChatThread::from).collect(),
            folders: folders.into_iter().map(FolderInfo::from).collect(),
        }
    }
}

/// A `thread` field read as the gateway's [`Thread`], held as the page's.
pub(super) fn thread_of<'de, D: Deserializer<'de>>(d: D) -> Result<ChatThread, D::Error> {
    Thread::deserialize(d).map(ChatThread::from)
}

/// `POST` something to a thread route that answers the thread (create, pin,
/// archive), and hold the answer as the page does.
pub(in crate::pages) async fn post_thread<B: Serialize>(
    path: impl Into<String>,
    body: &B,
) -> Result<ChatThread, crate::api::Error> {
    crate::api::post::<Thread, _>(path, body)
        .await
        .map(ChatThread::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat::{ContinueState, KbMode, ThreadDefaults};
    use lmgw_api_types::voice_resolved::VoiceResolved;
    use serde_json::json;

    fn row() -> ThreadRow {
        ThreadRow {
            id: 7,
            title: "Plans".into(),
            model_alias: "m".into(),
            temperature: Some(0.5),
            kb_mode: KbMode::Tool,
            kb_ids: vec![3],
            pinned: true,
            created_at: "2026-10-01 08:00:00".into(),
            updated_at: "2026-10-08 10:00:00".into(),
            last_message_at: Some(1_790_000_000),
            ..Default::default()
        }
    }

    /// Every field the page holds comes over, the knowledge mode as the
    /// text the page compares it by.
    #[test]
    fn a_row_becomes_the_page_s_thread_without_what_the_list_does_not_resolve() {
        let t = ChatThread::from(row());
        assert_eq!(
            (t.id, t.title.as_str(), t.model_alias.as_str()),
            (7, "Plans", "m")
        );
        assert_eq!((t.temperature, t.pinned), (Some(0.5), true));
        assert_eq!((t.kb_mode.as_str(), t.kb_ids.clone()), ("tool", vec![3]));
        assert_eq!(t.updated_at, "2026-10-08 10:00:00");
        assert_eq!((t.cont, t.voice_resolved), (None, None));
    }

    /// A thread read whole adds what its voice resolves to and its
    /// `continue`.
    #[test]
    fn a_whole_thread_carries_its_resolved_voice_and_continue() {
        let resolved = VoiceResolved {
            seed: Some(11),
            ..Default::default()
        };
        let verdict = ContinueState {
            ok: false,
            reason: Some("no reply".into()),
        };
        let t = ChatThread::from(Thread {
            row: row(),
            voice_resolved: resolved.clone(),
            continue_state: Some(verdict.clone()),
        });
        assert_eq!(t.id, 7);
        assert_eq!(t.voice_resolved, Some(resolved));
        assert_eq!(t.cont, Some(verdict));
        // Without a `continue` (where the thread is not read whole).
        let t = ChatThread::from(Thread {
            row: row(),
            ..Default::default()
        });
        assert_eq!(t.cont, None);
    }

    /// The page's `PartialEq` is the one the follower's decisions stand on
    /// (`chat_sync`'s `row_take` and `rows::merge`, which pin it again): the
    /// thread's times are not part of it, everything else the page shows
    /// is. `created_at` and `last_message_at` are the row's two fields the
    /// page leaves out, on purpose.
    #[test]
    fn two_rows_that_differ_only_in_what_the_page_leaves_out_are_the_same_thread() {
        let held = ChatThread::from(row());
        let later = ChatThread::from(ThreadRow {
            created_at: "2026-09-01 00:00:00".into(),
            last_message_at: Some(1_790_000_999),
            ..row()
        });
        assert_eq!(held, later);

        // Anything else the page shows is a difference, `updated_at` too.
        for changed in [
            ThreadRow {
                updated_at: "2026-10-08 10:05:00".into(),
                ..row()
            },
            ThreadRow {
                title: "Other".into(),
                ..row()
            },
            ThreadRow {
                pinned: false,
                ..row()
            },
            ThreadRow {
                kb_mode: KbMode::Auto,
                ..row()
            },
            ThreadRow {
                archived_at: Some("2026-10-09 00:00:00".into()),
                ..row()
            },
        ] {
            assert_ne!(held, ChatThread::from(changed));
        }
    }

    #[test]
    fn the_list_is_read_as_the_gateway_s_and_its_folders_keep_their_defaults_as_json() {
        let list: ThreadList = serde_json::from_value(json!({
            "threads": [{"id": 2, "title": "a", "kb_mode": "auto"}],
            "archived_count": 4,
            "temporary": [{"id": -1, "temporary": true}],
            "folders": [{
                "id": 3, "name": "F", "sort": 1, "threads_active": 2, "threads_archived": 1,
                "defaults": {"model_alias": "x", "top_k": 5},
                "ongoing": {"idle_minutes": 30, "current_thread_id": 12},
                "archive_days": null, "purge_days": 365, "devices_hidden": true,
            }],
        }))
        .unwrap();
        let r = ThreadsResponse::from(list);
        assert_eq!(r.threads.len(), 1);
        assert_eq!((r.threads[0].id, r.archived_count), (2, 4));
        assert!(r.temporary[0].temporary && r.temporary[0].id == -1);
        let f = &r.folders[0];
        assert_eq!((f.id, f.name.as_str(), f.sort), (3, "F", 1));
        assert_eq!((f.threads_active, f.threads_archived), (2, 1));
        assert_eq!((f.archive_days, f.purge_days), (None, Some(365)));
        assert!(f.devices_hidden);
        assert_eq!(
            f.ongoing.as_ref().map(|o| o.current_thread_id),
            Some(Some(12))
        );
        // The form reads these into a thread by name, every key present.
        assert_eq!(
            f.defaults,
            serde_json::to_value(ThreadDefaults {
                model_alias: Some("x".into()),
                top_k: Some(5),
                ..Default::default()
            })
            .unwrap()
        );
    }

    /// `ThreadDetail` holds the thread it reads as the gateway's `Thread`.
    #[test]
    fn a_detail_reads_its_thread_as_the_gateway_s_and_its_messages_as_the_page_s() {
        let d: super::super::ThreadDetail = serde_json::from_value(json!({
            "thread": {"id": 7, "title": "T", "voice_resolved": {"seed": 5},
                       "continue": {"ok": true, "reason": null}},
            "messages": [{"id": 1, "role": "user", "content": "hi",
                          "voice": {"via": 3}}],
            "draft_attachments": [{"id": 9, "kind": "pdf", "name": "a.pdf"}],
            "tasks": [],
        }))
        .unwrap();
        assert_eq!(d.thread.id, 7);
        assert_eq!(
            d.thread.cont,
            Some(ContinueState {
                ok: true,
                reason: None
            })
        );
        assert_eq!(d.thread.voice_resolved.map(|v| v.seed), Some(Some(5)));
        // A voice this build cannot read is a typed turn, not a thread that
        // fails to open.
        assert!(d.messages[0].voice.is_none());
        assert_eq!(d.draft_attachments[0].name, "a.pdf");
    }
}
