//! The Chat's repository seam (chat-complete design §7): where a thread's
//! settings, messages and attachments are read and written — the DB for an
//! ordinary thread, the gateway's memory ([`super::chat_temp`]) for a
//! temporary one.
//!
//! Every Chat handler goes through this rather than calling the store's chat
//! functions itself: `send`, the turn's persist step ([`super::chat_turn`]),
//! the tool loop's ([`super::agentchat`]), the message actions, the
//! attachment routes. That is what lets a temporary thread do everything an
//! ordinary one does without a second copy of any handler — and what a new
//! backing (or a new column) has to extend in one place.
//!
//! Which backing an id belongs to is its sign ([`ChatRepo::of`]): temporary
//! ids count down from −1, the DB's count up from 1.
//!
//! Every write here that rewrites a thread's history — a user message, an
//! edit, a delete, a truncation, the thread going away — goes through the
//! thread's [`LiveTurns`](super::chat_live::LiveTurns) lock and moves its
//! generation, so a turn that started before it cannot save its reply onto
//! the history it changed (review R1 finding 1). A reply itself is saved with
//! [`ChatRepo::save_reply`] / [`ChatRepo::save_continue`], under the lock the
//! turn's ticket holds.
//!
//! Every message write of a stored thread that went through marks the thread
//! ([`marked`]): the dashboard's `chat` frame names it, so a Chat page open
//! on it reads its messages again (`chat_feed::dashboard`).

use super::chat_caller::Caller;
use super::chat_live::{HistoryWrite, SaveGuard};
use super::chat_temp::TakeForKeep;
use crate::error::GatewayError;
use crate::state::AppState;
use crate::store::{
    self, ChatAttachmentFull, ChatAttachmentMeta, ChatContext, ChatMessageRow, ChatMessageUpdate,
    ChatReply, ChatThread, ContinueSave, DbResult, DeleteAttachmentOutcome, MessageVoice,
    NewAttachment, SeedWrite, SendMessageOutcome, SetModeOutcome,
};

/// What [`ChatRepo::keep`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeepOutcome {
    /// Written to the DB under this new id.
    Kept(i64),
    /// Another Keep of the same thread is running.
    Busy,
    /// A realtime session is bound to it (chat-voice design §8.1): it
    /// writes by the temporary id, so Keep waits until voice mode ends.
    VoiceActive,
    NotFound,
}

/// Where a thread lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChatRepo {
    /// An ordinary thread, in `chat_threads` / `chat_messages` /
    /// `chat_attachments`.
    Db,
    /// A temporary thread, in `state.chat_temp`.
    Temp,
}

/// A write into a temporary thread that has gone meanwhile — discarded, or
/// kept, while a reply was still streaming into it.
fn stored_only(what: &str) -> GatewayError {
    GatewayError::BadRequest(format!("a temporary chat cannot be {what}"))
}

fn gone() -> GatewayError {
    GatewayError::NotFound("this temporary chat was discarded or kept meanwhile".into())
}

/// A message write of stored thread `thread_id`, passed through: once it
/// went through, the thread is marked for the dashboard's `chat` frame
/// (`chat_feed::Feed::messages_changed`). A write that matched nothing marks
/// it too; the page that reads it again finds it as it was.
fn marked<T>(s: &AppState, thread_id: i64, written: DbResult<T>) -> DbResult<T> {
    if written.is_ok() {
        s.chat_feed.messages_changed(thread_id);
    }
    written
}

impl ChatRepo {
    /// The backing of a thread, message or attachment id: negative ids are
    /// temporary.
    pub(super) fn of(id: i64) -> Self {
        if id < 0 {
            Self::Temp
        } else {
            Self::Db
        }
    }

    pub(super) fn is_temp(self) -> bool {
        self == Self::Temp
    }

    // -- threads ------------------------------------------------------------

    /// A new thread, as created. A temporary one is always `chat` kind.
    pub(super) async fn create_thread(
        self,
        s: &AppState,
        model_alias: &str,
        kind: &str,
        system_prompt: &str,
        by: &Caller,
    ) -> DbResult<ChatThread> {
        match self {
            Self::Temp => Ok(s.chat_temp.create(model_alias, kind, system_prompt)),
            Self::Db => {
                let by = by.named();
                let id = store::create_chat_thread_with_prompt(
                    &s.db,
                    model_alias,
                    kind,
                    system_prompt,
                    Some(&by),
                )
                .await?;
                s.chat_feed.wake();
                store::get_chat_thread(&s.db, id).await?.ok_or_else(|| {
                    GatewayError::Internal(
                        "the thread vanished immediately after being created".into(),
                    )
                })
            }
        }
    }

    pub(super) async fn thread(self, s: &AppState, id: i64) -> DbResult<Option<ChatThread>> {
        match self {
            Self::Temp => Ok(s.chat_temp.thread(id)),
            Self::Db => store::get_chat_thread(&s.db, id).await,
        }
    }

    /// Thread `id` as `caller` may reach it (client-apps design L3): `None`
    /// for a thread a device does not see (Admin Chat; the self-admin
    /// toolset unless it may use lmgw's admin tools), exactly as for one
    /// that is not there. Every route that reaches a thread by its id, a message
    /// id or an attachment id resolves it here.
    ///
    /// A thread in a folder `caller` cannot see (review W4-7) comes without
    /// its folder: for a device the folder does not exist, so the thread is
    /// in none.
    pub(super) async fn thread_as(
        self,
        s: &AppState,
        caller: &Caller,
        id: i64,
    ) -> DbResult<Option<ChatThread>> {
        let snap = s.snapshot();
        let Some(mut t) = self.thread(s, id).await?.filter(|t| caller.sees(&snap, t)) else {
            return Ok(None);
        };
        if let (true, Some(folder)) = (caller.is_device(), t.folder_id) {
            let hidden = store::get_chat_folder(&s.db, folder)
                .await?
                .is_some_and(|f| !caller.sees_folder(&snap, &f));
            if hidden {
                t.folder_id = None;
            }
        }
        Ok(Some(t))
    }

    /// For a device, the threads of `threads` in a folder it cannot see lose
    /// their folder (review W4-7), as [`Self::thread_as`] does.
    pub(super) async fn hide_folders(
        s: &AppState,
        caller: &Caller,
        threads: &mut [ChatThread],
    ) -> DbResult<()> {
        if !caller.is_device() {
            return Ok(());
        }
        let hidden = store::self_admin_folder_ids(&s.db, caller.reach(&s.snapshot())).await?;
        for t in threads.iter_mut() {
            if t.folder_id.is_some_and(|f| hidden.contains(&f)) {
                t.folder_id = None;
            }
        }
        Ok(())
    }

    /// Whether attachment `id` belongs to a thread `caller` may reach (L3) —
    /// `false` also when there is no such attachment. A temporary thread is
    /// resolved like a stored one (review W4-2): the owner can attach the
    /// self-admin toolset to it, and its attachments are then as out of a
    /// device's reach as the thread.
    pub(super) async fn attachment_reachable(
        self,
        s: &AppState,
        caller: &Caller,
        id: i64,
    ) -> DbResult<bool> {
        if !caller.is_device() {
            return Ok(true);
        }
        let thread_id = match self {
            Self::Temp => s.chat_temp.attachment_thread(id),
            Self::Db => store::chat_attachment_thread_id(&s.db, id).await?,
        };
        let Some(thread_id) = thread_id else {
            return Ok(false);
        };
        Ok(Self::of(thread_id)
            .thread_as(s, caller, thread_id)
            .await?
            .is_some())
    }

    /// Every temporary thread, most recently active first — the thread
    /// list's `temporary` array.
    pub(super) fn temporary_threads(s: &AppState) -> Vec<ChatThread> {
        s.chat_temp.list()
    }

    /// The stored threads `caller` may see (L3), in `mode`'s order.
    pub(super) async fn stored_threads(
        s: &AppState,
        caller: &Caller,
        mode: store::ThreadListMode,
    ) -> DbResult<Vec<ChatThread>> {
        let reach = caller.reach(&s.snapshot());
        let mut threads = store::list_chat_threads_as(&s.db, mode, reach).await?;
        Self::hide_folders(s, caller, &mut threads).await?;
        Ok(threads)
    }

    /// The stored threads among `ids` that `caller` may see (L3), active and
    /// archived alike, in the active list's order.
    pub(super) async fn stored_rows(
        s: &AppState,
        caller: &Caller,
        ids: &[i64],
    ) -> DbResult<Vec<ChatThread>> {
        let reach = caller.reach(&s.snapshot());
        let mut threads = store::list_chat_threads_by_ids(&s.db, ids, reach).await?;
        Self::hide_folders(s, caller, &mut threads).await?;
        Ok(threads)
    }

    /// How many of the stored threads `caller` may see are archived.
    pub(super) async fn archived_count(s: &AppState, caller: &Caller) -> DbResult<i64> {
        store::count_archived_chat_threads(&s.db, caller.reach(&s.snapshot())).await
    }

    /// Pin or unpin (pinning an archived thread restores it). Stored threads
    /// only: a temporary one has nothing to pin until it is kept.
    pub(super) async fn set_pinned(
        self,
        s: &AppState,
        id: i64,
        pinned: bool,
        by: &Caller,
    ) -> DbResult<()> {
        match self {
            Self::Temp => Err(stored_only("pinned")),
            Self::Db => {
                store::set_chat_thread_pinned(&s.db, id, pinned, Some(&by.named())).await?;
                s.chat_feed.wake();
                Ok(())
            }
        }
    }

    /// Archive by hand, or restore. Stored threads only: a temporary one is
    /// discarded, never archived.
    pub(super) async fn set_archived(
        self,
        s: &AppState,
        id: i64,
        archived: bool,
        by: &Caller,
    ) -> DbResult<()> {
        let by = by.named();
        match self {
            Self::Temp => return Err(stored_only("archived")),
            Self::Db if archived => store::archive_chat_thread(&s.db, id, Some(&by)).await?,
            Self::Db => store::restore_chat_thread(&s.db, id, Some(&by)).await?,
        }
        s.chat_feed.wake();
        Ok(())
    }

    /// **Keep** a temporary thread: write it to the DB whole — settings,
    /// messages in order, attachments bound to the same messages — as an
    /// ordinary thread, then discard it from memory. The thread leaves memory
    /// *before* the insert (and comes back should it fail, or should this
    /// request be dropped halfway), so a second Keep running alongside is
    /// refused rather than storing it twice (review R1 finding 8). A reply
    /// still being written into it is cancelled.
    pub(super) async fn keep(s: &AppState, id: i64, by: &Caller) -> DbResult<KeepOutcome> {
        if s.chat_live.voice_bound(id) {
            return Ok(KeepOutcome::VoiceActive);
        }
        let mut _write = s.chat_live.discard(id).await;
        // A reply still being written into it is cancelled up front: the
        // thread leaves memory now.
        s.chat_live.discarded(&mut _write);
        let t = match s.chat_temp.take_for_keep(id) {
            TakeForKeep::Taken(t) => t,
            TakeForKeep::Busy => return Ok(KeepOutcome::Busy),
            TakeForKeep::NotFound => return Ok(KeepOutcome::NotFound),
        };
        /// Puts the thread back unless the insert went through.
        struct Taken<'a> {
            s: &'a AppState,
            t: Option<Box<super::chat_temp::TempThread>>,
        }
        impl Drop for Taken<'_> {
            fn drop(&mut self) {
                if let Some(t) = self.t.take() {
                    self.s.chat_temp.keep_failed(*t);
                }
            }
        }
        let mut taken = Taken { s, t: Some(t) };
        let t = taken.t.as_ref().expect("just put there");
        let attachments: Vec<store::KeptAttachment> =
            t.attachments.iter().map(|a| a.kept()).collect();
        let by = by.named();
        let new_id =
            store::insert_kept_chat_thread(&s.db, &t.thread, &t.messages, &attachments, Some(&by))
                .await?;
        s.chat_feed.wake();
        taken.t = None;
        s.chat_temp.keep_done(id);
        Ok(KeepOutcome::Kept(new_id))
    }

    /// Draw the thread's TTS seed on first use (chat-voice §2.2, §6.1):
    /// `drawn` is stored only where none is, so two first uses keep one. The
    /// seed in effect.
    pub(super) async fn draw_seed(self, s: &AppState, id: i64, drawn: u32) -> DbResult<u32> {
        match self {
            Self::Temp => s.chat_temp.draw_seed(id, drawn).ok_or_else(gone),
            Self::Db => {
                let seed = store::draw_chat_thread_seed(&s.db, id, drawn).await?;
                s.chat_feed.wake();
                seed.ok_or_else(|| GatewayError::NotFound("thread not found".into()))
            }
        }
    }

    /// Write the editable settings `t` holds to the thread `t.id`; `seed`
    /// says whether the stored seed stays (chat-voice §2.2). The voice as
    /// stored, and whether the thread drove the self-admin plane before and
    /// after, as the write saw it (review W4-3).
    pub(super) async fn update_settings(
        self,
        s: &AppState,
        t: &ChatThread,
        seed: SeedWrite,
        by: &Caller,
    ) -> DbResult<store::SettingsWritten> {
        let reach = by.reach(&s.snapshot());
        match self {
            Self::Temp => s.chat_temp.update_settings(t, seed, reach).ok_or_else(gone),
            Self::Db => {
                let written =
                    store::write_settings_of(&s.db, t, seed, Some(&by.named()), reach).await?;
                s.chat_feed.wake();
                written.ok_or_else(|| GatewayError::NotFound("thread not found".into()))
            }
        }
    }

    /// Whether `caller` may still write into thread `thread_id`, read under
    /// the thread's lock (review W4-3): a device's message never lands in a
    /// thread that became Admin Chat for it since the route resolved it.
    /// `NotFound` when it may not, as for a thread that is gone.
    async fn still_reachable(self, s: &AppState, caller: &Caller, thread_id: i64) -> DbResult<()> {
        if caller.is_device() && self.thread_as(s, caller, thread_id).await?.is_none() {
            return Err(GatewayError::NotFound("thread not found".into()));
        }
        Ok(())
    }

    pub(super) async fn set_title(
        self,
        s: &AppState,
        id: i64,
        title: &str,
        by: &Caller,
    ) -> DbResult<()> {
        match self {
            Self::Temp => {
                s.chat_temp.set_title(id, title);
                Ok(())
            }
            Self::Db => {
                store::set_chat_thread_title(&s.db, id, title, Some(&by.named())).await?;
                s.chat_feed.wake();
                Ok(())
            }
        }
    }

    /// Delete a thread with everything in it; a temporary one is discarded.
    /// A reply still being written into it is cancelled.
    pub(super) async fn delete_thread(
        self,
        s: &crate::state::SharedState,
        id: i64,
        by: &Caller,
    ) -> DbResult<()> {
        let held = s.chat_live.hold(id).await;
        self.delete_thread_held(s, id, by, held).await
    }

    /// [`Self::delete_thread`] under the thread's lock the caller holds
    /// already ([`super::chat_live::LiveTurns::hold`]), from its re-check of
    /// the thread's reach (review W6-6). Before the delete only the thread's
    /// live events leave every device's reach; once it committed, still
    /// under the lock, its turn is cancelled and a session a device bound
    /// to it closes, as one whose thread left its reach
    /// (`LiveTurns::discarded`). From the first step to the last on a task
    /// of its own, with the lock (`chat_live::to_its_end`): a client that
    /// hangs up meanwhile leaves no thread hidden from devices after a
    /// delete that failed, nor one deleted with its device's session still
    /// open.
    pub(super) async fn delete_thread_held(
        self,
        s: &crate::state::SharedState,
        id: i64,
        by: &Caller,
        held: super::chat_live::HistoryWrite,
    ) -> DbResult<()> {
        let (s, by) = (s.clone(), by.named());
        super::chat_live::to_its_end(async move {
            let mut held = held;
            s.chat_live.discard_held(&mut held);
            match self {
                Self::Temp => {
                    s.chat_temp.delete(id);
                }
                Self::Db => {
                    if let Err(e) = store::delete_chat_thread(&s.db, id, Some(&by)).await {
                        s.chat_live.delete_failed(&mut held);
                        return Err(e);
                    }
                    s.chat_feed.threads_deleted(&[id]);
                    s.chat_feed.wake();
                }
            }
            s.chat_live.discarded(&mut held);
            drop(held);
            Ok(())
        })
        .await?
    }

    /// A turn into an archived thread restores it (chat-archive design §1).
    /// A temporary thread is never archived.
    pub(super) async fn wake(self, s: &AppState, t: &ChatThread, by: &Caller) -> DbResult<()> {
        match self {
            Self::Db if t.archived_at.is_some() => {
                store::restore_chat_thread(&s.db, t.id, Some(&by.named())).await?;
                s.chat_feed.wake();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    // -- messages -----------------------------------------------------------

    /// The thread's messages in conversation order.
    pub(super) async fn messages(
        self,
        s: &AppState,
        thread_id: i64,
    ) -> DbResult<Vec<ChatMessageRow>> {
        match self {
            Self::Temp => Ok(s.chat_temp.messages(thread_id)),
            Self::Db => store::list_chat_messages(&s.db, thread_id).await,
        }
    }

    /// One message of this thread — `None` also when `id` is another
    /// thread's.
    pub(super) async fn message(
        self,
        s: &AppState,
        thread_id: i64,
        id: i64,
    ) -> DbResult<Option<ChatMessageRow>> {
        match self {
            Self::Temp => Ok(s.chat_temp.message(thread_id, id)),
            Self::Db => store::get_chat_message(&s.db, thread_id, id).await,
        }
    }

    pub(super) async fn last_message(
        self,
        s: &AppState,
        thread_id: i64,
    ) -> DbResult<Option<ChatMessageRow>> {
        match self {
            Self::Temp => Ok(s.chat_temp.last_message(thread_id)),
            Self::Db => store::last_chat_message(&s.db, thread_id).await,
        }
    }

    /// A user turn with its drafts bound to it, all or nothing, naming the
    /// knowledge bases picked for it alone (`kb_refs`) and, for a spoken
    /// turn, how it was spoken (`voice`, chat-voice design §3).
    // The message's own parts, and who writes it (review W4-3).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn append_user_message(
        self,
        s: &AppState,
        thread_id: i64,
        content: &str,
        attachment_ids: &[i64],
        kb_refs: &[i64],
        voice: Option<&MessageVoice>,
        caller: &Caller,
    ) -> DbResult<SendMessageOutcome> {
        let _write = s.chat_live.write(thread_id).await;
        self.still_reachable(s, caller, thread_id).await?;
        match self {
            Self::Temp => s
                .chat_temp
                .append_user_message(thread_id, content, attachment_ids, kb_refs, voice)
                .ok_or_else(gone),
            Self::Db => marked(
                s,
                thread_id,
                store::append_user_message_with_voice(
                    &s.db,
                    thread_id,
                    content,
                    attachment_ids,
                    kb_refs,
                    voice,
                )
                .await,
            ),
        }
    }

    /// A heard voice turn's user row (voice-audio-input design §3.3), under
    /// the thread's conditional write `_proof` (`LiveTurns::write_if`): an
    /// insert that moves no generation, so the reply of the turn that heard
    /// it is still saved after it.
    pub(super) async fn append_spoken_user(
        self,
        s: &AppState,
        _proof: &HistoryWrite,
        thread_id: i64,
        content: &str,
        voice: &MessageVoice,
        caller: &Caller,
    ) -> DbResult<SendMessageOutcome> {
        self.still_reachable(s, caller, thread_id).await?;
        match self {
            Self::Temp => s
                .chat_temp
                .append_user_message(thread_id, content, &[], &[], Some(voice))
                .ok_or_else(gone),
            Self::Db => marked(
                s,
                thread_id,
                store::append_user_message_with_voice(
                    &s.db,
                    thread_id,
                    content,
                    &[],
                    &[],
                    Some(voice),
                )
                .await,
            ),
        }
    }

    /// Save a turn's reply as a new assistant row; its id. Only with the
    /// turn's [`SaveGuard`] — the proof its history has not moved since it
    /// started ([`super::chat_live::Ticket::save_lock`]) — and, for a
    /// device's turn, while the thread is still in its reach (review W5-18):
    /// the guard holds the thread's lock, which the attach of the
    /// self-admin toolset takes too, so the check and the write are one.
    pub(super) async fn save_reply(
        self,
        s: &AppState,
        _proof: &SaveGuard,
        caller: &Caller,
        thread_id: i64,
        r: &ChatReply,
    ) -> DbResult<i64> {
        self.still_reachable(s, caller, thread_id).await?;
        match self {
            Self::Temp => s.chat_temp.append_reply(thread_id, r).ok_or_else(gone),
            Self::Db => marked(
                s,
                thread_id,
                store::append_chat_reply(&s.db, thread_id, r).await,
            ),
        }
    }

    /// Save a continue onto reply `id` — only while its text is still
    /// `prefix` (re-read inside the write), only with the turn's
    /// [`SaveGuard`], and for a device only while the thread is in its reach
    /// ([`Self::save_reply`]).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn save_continue(
        self,
        s: &AppState,
        _proof: &SaveGuard,
        caller: &Caller,
        thread_id: i64,
        id: i64,
        prefix: &str,
        r: &ChatReply,
    ) -> DbResult<ContinueSave> {
        self.still_reachable(s, caller, thread_id).await?;
        match self {
            Self::Temp => Ok(s.chat_temp.continue_reply(thread_id, id, prefix, r)),
            Self::Db => marked(
                s,
                thread_id,
                store::continue_chat_reply(&s.db, thread_id, id, prefix, r).await,
            ),
        }
    }

    /// Rewrite a message in place; `false` when it is not in this thread.
    pub(super) async fn update_message(
        self,
        s: &AppState,
        thread_id: i64,
        id: i64,
        m: &ChatMessageUpdate,
    ) -> DbResult<bool> {
        let _write = s.chat_live.write(thread_id).await;
        match self {
            Self::Temp => Ok(s.chat_temp.update_message(thread_id, id, m)),
            Self::Db => marked(
                s,
                thread_id,
                store::update_chat_message(&s.db, thread_id, id, m).await,
            ),
        }
    }

    /// Rewrite user message `id` for a resend, as one write (review R1
    /// finding 9): its text, its knowledge picks, its stored retrieval
    /// cleared, and every later message deleted. `false` — nothing written —
    /// when it is not a user message of this thread.
    pub(super) async fn rewrite_user_message(
        self,
        s: &AppState,
        thread_id: i64,
        id: i64,
        content: &str,
        kb_refs: &[i64],
        caller: &Caller,
    ) -> DbResult<bool> {
        let _write = s.chat_live.write(thread_id).await;
        self.still_reachable(s, caller, thread_id).await?;
        match self {
            Self::Temp => Ok(s
                .chat_temp
                .rewrite_user_message(thread_id, id, content, kb_refs)),
            Self::Db => marked(
                s,
                thread_id,
                store::rewrite_chat_user_message(&s.db, thread_id, id, content, kb_refs).await,
            ),
        }
    }

    /// Set a message's `kb_refs` and its stored retrieval (`context`)
    /// together; `false` when it is not in this thread. Not a rewrite of the
    /// history: a turn's own retrieval stores itself with this.
    pub(super) async fn set_message_knowledge(
        self,
        s: &AppState,
        thread_id: i64,
        id: i64,
        kb_refs: &[i64],
        context: Option<&ChatContext>,
    ) -> DbResult<bool> {
        match self {
            Self::Temp => Ok(s
                .chat_temp
                .set_message_knowledge(thread_id, id, kb_refs, context)),
            Self::Db => marked(
                s,
                thread_id,
                store::set_chat_message_knowledge(&s.db, thread_id, id, kb_refs, context).await,
            ),
        }
    }

    /// Delete one message and its attachments; `false` when it is not in
    /// this thread.
    pub(super) async fn delete_message(
        self,
        s: &AppState,
        thread_id: i64,
        id: i64,
    ) -> DbResult<bool> {
        let _write = s.chat_live.write(thread_id).await;
        match self {
            Self::Temp => Ok(s.chat_temp.delete_message(thread_id, id)),
            Self::Db => marked(
                s,
                thread_id,
                store::delete_chat_message(&s.db, thread_id, id).await,
            ),
        }
    }

    /// Annotate a spoken reply (chat-voice design §8.3): its `voice` alone.
    /// Not a rewrite of the history: no generation moves, no turn is
    /// cancelled. `false` when it is not in this thread.
    pub(super) async fn set_message_voice(
        self,
        s: &AppState,
        thread_id: i64,
        id: i64,
        voice: &MessageVoice,
    ) -> DbResult<bool> {
        match self {
            Self::Temp => Ok(s.chat_temp.set_message_voice(thread_id, id, voice)),
            Self::Db => marked(
                s,
                thread_id,
                store::set_chat_message_voice(&s.db, thread_id, id, voice).await,
            ),
        }
    }

    /// Cut a spoken reply to what was heard (§8.3), under the thread's
    /// conditional write `_proof` (`LiveTurns::write_if`).
    pub(super) async fn cut_reply(
        self,
        s: &AppState,
        _proof: &HistoryWrite,
        thread_id: i64,
        id: i64,
        content: &str,
        voice: &MessageVoice,
    ) -> DbResult<bool> {
        match self {
            Self::Temp => Ok(s.chat_temp.cut_reply(thread_id, id, content, voice)),
            Self::Db => marked(
                s,
                thread_id,
                store::cut_chat_reply(&s.db, thread_id, id, content, voice).await,
            ),
        }
    }

    /// Delete a spoken reply nobody heard (§8.3), under the thread's
    /// conditional write `_proof`.
    pub(super) async fn delete_unheard(
        self,
        s: &AppState,
        _proof: &HistoryWrite,
        thread_id: i64,
        id: i64,
    ) -> DbResult<bool> {
        match self {
            Self::Temp => Ok(s.chat_temp.delete_message(thread_id, id)),
            Self::Db => marked(
                s,
                thread_id,
                store::delete_chat_message(&s.db, thread_id, id).await,
            ),
        }
    }

    /// Delete every message after `id` (and `id` itself when `inclusive`),
    /// with their attachments; how many went.
    pub(super) async fn truncate(
        self,
        s: &AppState,
        thread_id: i64,
        id: i64,
        inclusive: bool,
    ) -> DbResult<u64> {
        let _write = s.chat_live.write(thread_id).await;
        match self {
            Self::Temp => Ok(s.chat_temp.truncate(thread_id, id, inclusive)),
            Self::Db => marked(
                s,
                thread_id,
                store::truncate_chat_messages(&s.db, thread_id, id, inclusive).await,
            ),
        }
    }

    // -- attachments --------------------------------------------------------

    /// Store an upload as a draft of this thread; its id.
    pub(super) async fn insert_attachment(
        self,
        s: &AppState,
        thread_id: i64,
        a: &NewAttachment,
    ) -> DbResult<i64> {
        match self {
            Self::Temp => s.chat_temp.insert_attachment(thread_id, a).ok_or_else(gone),
            Self::Db => store::insert_chat_attachment_new(&s.db, thread_id, a).await,
        }
    }

    /// Store text derived after the upload (a transcript made on first need).
    pub(super) async fn set_extracted(
        self,
        s: &AppState,
        id: i64,
        text: &str,
        meta: &serde_json::Value,
    ) -> DbResult<()> {
        match self {
            Self::Temp => {
                s.chat_temp.set_extracted(id, text, meta);
                Ok(())
            }
            Self::Db => store::set_chat_attachment_extracted(&s.db, id, text, meta).await,
        }
    }

    /// Set a text-class PDF's mode; drafts only.
    pub(super) async fn set_mode(
        self,
        s: &AppState,
        id: i64,
        mode: &str,
    ) -> DbResult<SetModeOutcome> {
        match self {
            Self::Temp => Ok(s.chat_temp.set_mode(id, mode)),
            Self::Db => store::set_chat_attachment_mode(&s.db, id, mode).await,
        }
    }

    /// A cached page image of a PDF attachment.
    pub(super) async fn page_png(
        self,
        s: &AppState,
        id: i64,
        page: u32,
    ) -> DbResult<Option<Vec<u8>>> {
        match self {
            Self::Temp => Ok(s.chat_temp.page(id, page)),
            Self::Db => store::get_chat_attachment_page(&s.db, id, page).await,
        }
    }

    pub(super) async fn put_page_png(
        self,
        s: &AppState,
        id: i64,
        page: u32,
        png: &[u8],
    ) -> DbResult<()> {
        match self {
            Self::Temp => {
                s.chat_temp.put_page(id, page, png);
                Ok(())
            }
            Self::Db => store::put_chat_attachment_page(&s.db, id, page, png).await,
        }
    }

    /// Every attachment of the thread, metadata only.
    pub(super) async fn attachments_meta(
        self,
        s: &AppState,
        thread_id: i64,
    ) -> DbResult<Vec<ChatAttachmentMeta>> {
        match self {
            Self::Temp => Ok(s.chat_temp.attachments_meta(thread_id)),
            Self::Db => store::list_chat_attachments_meta(&s.db, thread_id).await,
        }
    }

    /// Each attachment's `(ord, created_at)`, by id.
    pub(super) async fn attachment_order(
        self,
        s: &AppState,
        thread_id: i64,
    ) -> DbResult<std::collections::HashMap<i64, (i64, String)>> {
        match self {
            Self::Temp => Ok(s.chat_temp.attachment_order(thread_id)),
            Self::Db => store::chat_attachment_order(&s.db, thread_id).await,
        }
    }

    /// Every sent attachment with its bytes (see
    /// [`store::list_sent_chat_attachments`] for `vision`).
    pub(super) async fn sent_attachments(
        self,
        s: &AppState,
        thread_id: i64,
        vision: Option<bool>,
    ) -> DbResult<Vec<ChatAttachmentFull>> {
        match self {
            Self::Temp => Ok(s.chat_temp.sent_attachments(thread_id, vision)),
            Self::Db => store::list_sent_chat_attachments(&s.db, thread_id, vision).await,
        }
    }

    /// The drafts of this thread among `ids`.
    pub(super) async fn drafts_by_ids(
        self,
        s: &AppState,
        thread_id: i64,
        ids: &[i64],
    ) -> DbResult<Vec<ChatAttachmentMeta>> {
        match self {
            Self::Temp => Ok(s.chat_temp.drafts_by_ids(thread_id, ids)),
            Self::Db => store::list_draft_chat_attachments_by_ids(&s.db, thread_id, ids).await,
        }
    }

    /// One attachment with its bytes, by its own id (the repo is the
    /// attachment id's, not a thread's).
    pub(super) async fn attachment(
        self,
        s: &AppState,
        id: i64,
    ) -> DbResult<Option<ChatAttachmentFull>> {
        match self {
            Self::Temp => Ok(s.chat_temp.attachment(id)),
            Self::Db => store::get_chat_attachment_full(&s.db, id).await,
        }
    }

    /// Delete a draft by its own id; a sent one is refused.
    pub(super) async fn delete_draft(
        self,
        s: &AppState,
        id: i64,
    ) -> DbResult<DeleteAttachmentOutcome> {
        match self {
            Self::Temp => Ok(s.chat_temp.delete_draft(id)),
            Self::Db => store::delete_draft_chat_attachment(&s.db, id).await,
        }
    }
}

#[cfg(test)]
mod marks_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings write onto a thread that went away after it was read is
    /// a `NotFound`, which the settings route answers as its 404 — on both
    /// backings.
    #[tokio::test]
    async fn a_settings_write_onto_a_vanished_thread_is_not_found() {
        let state = AppState::init_for_tests().await.unwrap();
        let tid = store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        let repo = ChatRepo::of(tid);
        let thread = repo.thread(&state, tid).await.unwrap().unwrap();
        let owner = Caller::default();
        repo.delete_thread(&state, tid, &owner).await.unwrap();
        let written = repo
            .update_settings(&state, &thread, SeedWrite::Keep, &owner)
            .await
            .map(|w| w.voice);
        assert!(
            matches!(written, Err(GatewayError::NotFound(_))),
            "{written:?}"
        );

        let gone = ChatThread { id: -7, ..thread };
        let written = ChatRepo::of(gone.id)
            .update_settings(&state, &gone, SeedWrite::Keep, &owner)
            .await
            .map(|w| w.voice);
        assert!(
            matches!(written, Err(GatewayError::NotFound(_))),
            "{written:?}"
        );
    }
}
