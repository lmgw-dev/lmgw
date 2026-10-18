//! Every message write of a stored thread records its `message.*` change in
//! the change feed's table, in its own transaction (module doc, client-apps
//! design §2.2, WP10): each of the [`ChatRepo`] message writes leaves the
//! records it should — the kind, its thread, the messages it names, its
//! author — and no other `message.*` record. Its records replaced the
//! in-memory marks the dashboard's `chat` frame read before (§3.6).
//!
//! The other half of the guard is `tests/it/chat_repo_seam_scan.rs`: no
//! production code writes a message past this seam, and every store
//! function whose own SQL writes `chat_messages` records.

use super::*;
use crate::store::feed::{self as table, kind, BY_OWNER};

/// One `message.*` record: its kind, thread, messages and author.
type Rec = (String, i64, Vec<i64>, Option<String>);

/// What the writes need: a stored thread, the gateway, the owner, and the
/// table's head read so far.
struct Fixture {
    state: crate::state::SharedState,
    tid: i64,
    owner: Caller,
    seen: i64,
}

impl Fixture {
    async fn new() -> Self {
        let state = AppState::init_for_tests().await.unwrap();
        let tid = store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        let seen = table::bounds(&state.db).await.unwrap().head;
        Self {
            state,
            tid,
            owner: Caller::default(),
            seen,
        }
    }

    /// The `message.*` records written since the last read.
    async fn records(&mut self) -> Vec<Rec> {
        let page = table::page(&self.state.db, self.seen, 1000).await.unwrap();
        if let Some(last) = page.last() {
            self.seen = last.seq;
        }
        page.iter()
            .filter(|r| r.kind.starts_with("message."))
            .map(|r| {
                (
                    r.kind.clone(),
                    r.thread_id.unwrap(),
                    r.message_ids().unwrap(),
                    r.by.clone(),
                )
            })
            .collect()
    }

    /// The write `what` recorded exactly `want` (kind, messages), each in
    /// this thread and `by` `by`.
    async fn recorded(&mut self, what: &str, want: &[(&str, Vec<i64>)], by: Option<&str>) {
        let want: Vec<Rec> = want
            .iter()
            .map(|(k, ids)| (k.to_string(), self.tid, ids.clone(), by.map(str::to_string)))
            .collect();
        assert_eq!(self.records().await, want, "{what}");
    }

    fn repo(&self) -> ChatRepo {
        ChatRepo::of(self.tid)
    }

    async fn user(&self, text: &str) -> i64 {
        match self
            .repo()
            .append_user_message(&self.state, self.tid, text, &[], &[], None, &self.owner)
            .await
            .unwrap()
        {
            SendMessageOutcome::Sent(id) => id,
            SendMessageOutcome::AttachmentNotDraft => panic!("no attachments were named"),
        }
    }

    async fn reply(&self, text: &str) -> i64 {
        let ticket = self.state.chat_live.begin(self.tid).await;
        let proof = ticket.save_lock().await.unwrap();
        let r = ChatReply {
            content: text.into(),
            model: Some("m".into()),
            ..Default::default()
        };
        self.repo()
            .save_reply(&self.state, &proof, &self.owner, self.tid, &r)
            .await
            .unwrap()
    }
}

fn spoken() -> MessageVoice {
    MessageVoice {
        via: store::VIA_REALTIME.into(),
        ..Default::default()
    }
}

const ADDED: &str = kind::MESSAGE_ADDED;
const UPDATED: &str = kind::MESSAGE_UPDATED;
const DELETED: &str = kind::MESSAGE_DELETED;

#[tokio::test]
async fn each_message_write_records_its_messages_by_its_author() {
    let mut f = Fixture::new().await;
    let (s, tid) = (f.state.clone(), f.tid);
    let repo = f.repo();
    let me = Some(BY_OWNER);

    // A typed turn: the user row, then the saved reply, each its own record.
    let user = f.user("hello").await;
    f.recorded("append_user_message", &[(ADDED, vec![user])], me)
        .await;
    let reply = f.reply("Hi.").await;
    f.recorded("save_reply", &[(ADDED, vec![reply])], me).await;

    // Continue the reply.
    {
        let ticket = s.chat_live.begin(tid).await;
        let proof = ticket.save_lock().await.unwrap();
        let more = ChatReply {
            content: " And more.".into(),
            ..Default::default()
        };
        let saved = repo
            .save_continue(&s, &proof, &f.owner, tid, reply, "Hi.", &more)
            .await
            .unwrap();
        assert_eq!(saved, ContinueSave::Saved);
    }
    f.recorded("save_continue", &[(UPDATED, vec![reply])], me)
        .await;

    // Edit the reply in place.
    let edited = ChatMessageUpdate {
        content: "Edited.".into(),
        ..Default::default()
    };
    assert!(repo
        .update_message(&s, tid, reply, &edited, &f.owner)
        .await
        .unwrap());
    f.recorded("update_message", &[(UPDATED, vec![reply])], me)
        .await;

    // What the turn retrieved, stored with its user message.
    let context = ChatContext {
        query: "hello".into(),
        ..Default::default()
    };
    assert!(repo
        .set_message_knowledge(&s, tid, user, &[], Some(&context), &f.owner)
        .await
        .unwrap());
    f.recorded("set_message_knowledge", &[(UPDATED, vec![user])], me)
        .await;

    // A spoken turn: its user row, its reply annotated, cut to what was
    // heard — by the session's binder.
    let spoken_user = {
        let held = s.chat_live.hold(tid).await;
        match repo
            .append_spoken_user(&s, &held, tid, "what time is it", &spoken(), &f.owner)
            .await
            .unwrap()
        {
            SendMessageOutcome::Sent(id) => id,
            SendMessageOutcome::AttachmentNotDraft => panic!("no attachments were named"),
        }
    };
    f.recorded("append_spoken_user", &[(ADDED, vec![spoken_user])], me)
        .await;
    let spoken_reply = f.reply("It is late. Very late.").await;
    f.recorded("save_reply (spoken)", &[(ADDED, vec![spoken_reply])], me)
        .await;
    let (device, by_phone) = (phone(), Some("device 'phone'"));
    assert!(repo
        .set_message_voice(&s, tid, spoken_reply, &spoken(), &device)
        .await
        .unwrap());
    f.recorded(
        "set_message_voice",
        &[(UPDATED, vec![spoken_reply])],
        by_phone,
    )
    .await;
    {
        let held = s.chat_live.hold(tid).await;
        assert!(repo
            .cut_reply(
                &s,
                &held,
                tid,
                spoken_reply,
                "It is late.",
                &spoken(),
                &device
            )
            .await
            .unwrap());
    }
    f.recorded("cut_reply", &[(UPDATED, vec![spoken_reply])], by_phone)
        .await;

    // A reply nobody heard goes.
    let unheard = f.reply("Unheard.").await;
    f.recorded("save_reply (unheard)", &[(ADDED, vec![unheard])], me)
        .await;
    {
        let held = s.chat_live.hold(tid).await;
        assert!(repo
            .delete_unheard(&s, &held, tid, unheard, &device)
            .await
            .unwrap());
    }
    f.recorded("delete_unheard", &[(DELETED, vec![unheard])], by_phone)
        .await;

    // A user message edited for a resend: it changed, what followed went.
    assert!(repo
        .rewrite_user_message(&s, tid, spoken_user, "what day is it", &[], &f.owner)
        .await
        .unwrap());
    f.recorded(
        "rewrite_user_message",
        &[(UPDATED, vec![spoken_user]), (DELETED, vec![spoken_reply])],
        me,
    )
    .await;

    // One message deleted, then everything after the first one.
    let last = f.reply("Monday.").await;
    f.recorded("save_reply (after the rewrite)", &[(ADDED, vec![last])], me)
        .await;
    assert!(repo.delete_message(&s, tid, last, &f.owner).await.unwrap());
    f.recorded("delete_message", &[(DELETED, vec![last])], me)
        .await;
    assert_eq!(
        repo.truncate(&s, tid, user, false, &f.owner).await.unwrap(),
        2
    );
    f.recorded("truncate", &[(DELETED, vec![reply, spoken_user])], me)
        .await;

    // A late MCP task result entering the thread (MCP Tasks design §3.1):
    // the gateway's own.
    let task = store::mcp_tasks::insert(
        &s.db,
        &store::mcp_tasks::NewMcpTask {
            server_id: 1,
            server_label: "desktop",
            task_id: "t1",
            thread_id: tid,
            tool: "desktop__build",
            call_id: "call_1",
            started_by: None,
            status: "working",
            status_message: None,
            poll_interval_ms: None,
            ttl_ms: None,
        },
    )
    .await
    .unwrap();
    let ended = store::mcp_tasks::Ended {
        status: "completed",
        status_message: None,
        result: r#"[{"type":"text","text":"job t1 (desktop__build) completed"}]"#,
        ended_by: None,
    };
    assert!(store::mcp_tasks::end(&s.db, task, &ended).await.unwrap());
    let delivered = repo.deliver_task(&s, tid, task).await.unwrap().unwrap();
    f.recorded("deliver_task", &[(ADDED, vec![delivered.message_id])], None)
        .await;

    let rows = repo.messages(&s, tid).await.unwrap();
    assert_eq!(
        rows.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![user, delivered.message_id],
        "the writes all went through"
    );
}

/// A paired device's caller, as the Chat API resolves its key.
fn phone() -> Caller {
    Caller::of(&crate::proxy::RequestCtx {
        principal: crate::principal::Principal::Key {
            id: 9,
            name: "device:phone".into(),
            kind: crate::config::ApiKeyKind::Device,
            agent_id: None,
            fingerprint: String::new(),
        },
        ..Default::default()
    })
}

/// A session's writes into a stored reply name the session that writes
/// them, whoever holds the thread now: the dashboard's session taken over by
/// the phone still drains its journal, and its cut, annotation and unheard
/// delete are the dashboard's, never the phone's (`chat_voice::bound`,
/// review of WP10).
#[tokio::test]
async fn a_taken_over_session_s_writes_name_their_own_session() {
    use super::super::chat_voice::bound::{annotate, cut_if, delete_if, Guarded};
    let mut f = Fixture::new().await;
    let (s, tid) = (f.state.clone(), f.tid);
    f.user("hello").await;
    let reply = f.reply("It is late. Very late.").await;
    let unheard = f.reply("Unheard.").await;
    f.records().await;
    // The phone holds the thread now.
    let _phone = s.chat_feed.live.voice_bound(tid, "device 'phone'", 0);
    let dashboard = Caller::default();
    assert!(annotate(&s, tid, reply, &spoken(), &dashboard).await);
    // The generation the voice turn saved at, as its journal guards with it
    // (the turn held, as the session's is while its journal drains).
    let turn = s.chat_live.begin(tid).await;
    let generation = turn.generation();
    assert_eq!(
        cut_if(
            &s,
            tid,
            generation,
            reply,
            "It is late.",
            &spoken(),
            &dashboard
        )
        .await,
        Guarded::Written(true)
    );
    assert_eq!(
        delete_if(&s, tid, generation, unheard, &dashboard).await,
        Guarded::Written(true)
    );
    f.recorded(
        "the dashboard's session, drained after the takeover",
        &[
            (UPDATED, vec![reply]),
            (UPDATED, vec![reply]),
            (DELETED, vec![unheard]),
        ],
        Some(BY_OWNER),
    )
    .await;
}

/// A write that failed, or matched nothing, records nothing; a temporary
/// thread is never in the table (L7); a thread deleted whole says so with
/// its `thread.deleted` alone.
#[tokio::test]
async fn a_failed_write_a_miss_a_temporary_thread_and_a_thread_delete_record_no_message() {
    let mut f = Fixture::new().await;
    let s = f.state.clone();
    let user = f.user("hello").await;
    f.recorded(
        "append_user_message",
        &[(ADDED, vec![user])],
        Some(BY_OWNER),
    )
    .await;

    // The write fails inside its transaction: the record goes with it.
    sqlx::query(
        "CREATE TRIGGER fail_edit BEFORE UPDATE ON chat_messages
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&s.db)
    .await
    .unwrap();
    let edit = ChatMessageUpdate {
        content: "x".into(),
        ..Default::default()
    };
    assert!(f
        .repo()
        .update_message(&s, f.tid, user, &edit, &f.owner)
        .await
        .is_err());
    assert_eq!(f.records().await, Vec::<Rec>::new(), "a failed write");
    sqlx::query("DROP TRIGGER fail_edit")
        .execute(&s.db)
        .await
        .unwrap();

    // Another thread's message, or none: nothing matched.
    assert!(!f
        .repo()
        .update_message(&s, f.tid, user + 1000, &edit, &f.owner)
        .await
        .unwrap());
    assert!(!f
        .repo()
        .delete_message(&s, f.tid, user + 1000, &f.owner)
        .await
        .unwrap());
    assert_eq!(
        f.repo()
            .truncate(&s, f.tid, user, false, &f.owner)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.records().await,
        Vec::<Rec>::new(),
        "a write that matched nothing"
    );

    let temp = ChatRepo::Temp
        .create_thread(&s, "m", "chat", "", None, &f.owner)
        .await
        .unwrap();
    ChatRepo::Temp
        .append_user_message(&s, temp.id, "hi", &[], &[], None, &f.owner)
        .await
        .unwrap();
    assert_eq!(f.records().await, Vec::<Rec>::new(), "a temporary thread");

    f.repo()
        .delete_thread(&f.state, f.tid, &f.owner)
        .await
        .unwrap();
    let page = table::page(&s.db, 0, 1000).await.unwrap();
    assert_eq!(
        page.last().map(|r| r.kind.as_str()),
        Some(kind::THREAD_DELETED)
    );
    assert_eq!(f.records().await, Vec::<Rec>::new(), "a thread delete");
}
