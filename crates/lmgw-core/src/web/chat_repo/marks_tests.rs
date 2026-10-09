//! Every message write of a stored thread marks it for the dashboard's
//! `chat` frame (module doc, client-apps design §3.6): each of the thirteen
//! [`ChatRepo`] message writes moves the marks for its own thread, once, by
//! itself (review CL-6). The user row's mark and the saved reply's are two
//! marks, so a page open on the thread reads it again for each.
//!
//! The other half of the guard is `tests/it/chat_repo_seam_scan.rs`: no
//! production code writes a message past this seam.

use super::*;

/// What the thirteen writes need: a stored thread, the gateway, the owner.
struct Fixture {
    state: crate::state::SharedState,
    tid: i64,
    owner: Caller,
    seen: u64,
}

impl Fixture {
    async fn new() -> Self {
        let state = AppState::init_for_tests().await.unwrap();
        let tid = store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        let (_, seen) = state.chat_feed.marked_since(0);
        Self {
            state,
            tid,
            owner: Caller::default(),
            seen,
        }
    }

    /// The write `what` marked this thread, and only it, once, since the
    /// last check.
    fn marked(&mut self, what: &str) {
        let (threads, now) = self.state.chat_feed.marked_since(self.seen);
        assert_eq!(threads, vec![self.tid], "{what}: the thread's mark");
        assert_eq!(now, self.seen + 1, "{what}: one mark, of its own");
        self.seen = now;
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

#[tokio::test]
async fn each_message_write_marks_its_thread_by_itself() {
    let mut f = Fixture::new().await;
    let (s, tid) = (f.state.clone(), f.tid);
    let repo = f.repo();

    // A typed turn: the user row, then the saved reply, each a mark.
    let user = f.user("hello").await;
    f.marked("append_user_message");
    let reply = f.reply("Hi.").await;
    f.marked("save_reply");

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
    f.marked("save_continue");

    // Edit the reply in place.
    let edited = ChatMessageUpdate {
        content: "Edited.".into(),
        ..Default::default()
    };
    assert!(repo.update_message(&s, tid, reply, &edited).await.unwrap());
    f.marked("update_message");

    // What the turn retrieved, stored with its user message.
    let context = ChatContext {
        query: "hello".into(),
        ..Default::default()
    };
    assert!(repo
        .set_message_knowledge(&s, tid, user, &[], Some(&context))
        .await
        .unwrap());
    f.marked("set_message_knowledge");

    // A spoken turn: its user row, its reply annotated, cut to what was
    // heard.
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
    f.marked("append_spoken_user");
    let spoken_reply = f.reply("It is late. Very late.").await;
    f.marked("save_reply (spoken)");
    assert!(repo
        .set_message_voice(&s, tid, spoken_reply, &spoken())
        .await
        .unwrap());
    f.marked("set_message_voice");
    {
        let held = s.chat_live.hold(tid).await;
        assert!(repo
            .cut_reply(&s, &held, tid, spoken_reply, "It is late.", &spoken())
            .await
            .unwrap());
    }
    f.marked("cut_reply");

    // A reply nobody heard goes.
    let unheard = f.reply("Unheard.").await;
    f.marked("save_reply (unheard)");
    {
        let held = s.chat_live.hold(tid).await;
        assert!(repo.delete_unheard(&s, &held, tid, unheard).await.unwrap());
    }
    f.marked("delete_unheard");

    // A user message edited for a resend.
    assert!(repo
        .rewrite_user_message(&s, tid, spoken_user, "what day is it", &[], &f.owner)
        .await
        .unwrap());
    f.marked("rewrite_user_message");

    // One message deleted, then everything after the first one.
    let last = f.reply("Monday.").await;
    f.marked("save_reply (after the rewrite)");
    assert!(repo.delete_message(&s, tid, last).await.unwrap());
    f.marked("delete_message");
    assert!(repo.truncate(&s, tid, user, false).await.unwrap() > 0);
    f.marked("truncate");

    // A late MCP task result entering the thread (MCP Tasks design §3.1).
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
    f.marked("deliver_task");

    let rows = repo.messages(&s, tid).await.unwrap();
    assert_eq!(
        rows.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![user, delivered.message_id],
        "the writes all went through"
    );
}

/// A write that failed marks nothing; a temporary thread is never marked.
#[tokio::test]
async fn a_failed_write_or_a_temporary_thread_marks_nothing() {
    let mut f = Fixture::new().await;
    let s = f.state.clone();
    let user = f.user("hello").await;
    f.marked("append_user_message");

    sqlx::query("ALTER TABLE chat_messages RENAME TO chat_messages_away")
        .execute(&s.db)
        .await
        .unwrap();
    let edit = ChatMessageUpdate {
        content: "x".into(),
        ..Default::default()
    };
    assert!(f
        .repo()
        .update_message(&s, f.tid, user, &edit)
        .await
        .is_err());
    assert_eq!(s.chat_feed.marked_since(f.seen).0, Vec::<i64>::new());
    sqlx::query("ALTER TABLE chat_messages_away RENAME TO chat_messages")
        .execute(&s.db)
        .await
        .unwrap();

    let temp = ChatRepo::Temp
        .create_thread(&s, "m", "chat", "", None, &f.owner)
        .await
        .unwrap();
    ChatRepo::Temp
        .append_user_message(&s, temp.id, "hi", &[], &[], None, &f.owner)
        .await
        .unwrap();
    assert_eq!(s.chat_feed.marked_since(f.seen).0, Vec::<i64>::new());
}

/// A deleted thread's mark goes with it (review CL-10).
#[tokio::test]
async fn a_deleted_thread_leaves_no_mark_behind() {
    let mut f = Fixture::new().await;
    f.user("hello").await;
    f.marked("append_user_message");
    f.repo()
        .delete_thread(&f.state, f.tid, &f.owner)
        .await
        .unwrap();
    assert_eq!(f.state.chat_feed.marked_since(0).0, Vec::<i64>::new());
}
