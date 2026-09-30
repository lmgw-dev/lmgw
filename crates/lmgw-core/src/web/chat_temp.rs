//! Temporary chats (chat-complete design §7): threads that are **never
//! written to the database**. They live here, in the gateway process, until
//! they are discarded, kept (written to the DB as an ordinary thread), or
//! lmgw exits.
//!
//! Threads, messages and attachments all take their ids from one counter
//! going down from −1, so an id's sign says where it lives and every
//! `/chat/api/threads/{id}/…` and `/chat/api/attachments/{id}` route
//! dispatches on it ([`super::chat_repo::ChatRepo::of`]). One counter per
//! gateway rather than a `static`, for the reason [`AppState`]'s other
//! in-memory desks give: two gateways in one test process must not see each
//! other's threads.
//!
//! The semantics mirror the DB's ([`crate::store`]'s chat functions)
//! operation for operation, so the handlers cannot tell the two apart:
//! messages keep the conversation's order (a `Vec`, since the ids run
//! backwards), a write bumps the thread's `updated_at`, deleting a message
//! takes its attachments with it, and a draft binds to a message in the order
//! the send listed it.
//!
//! [`AppState`]: crate::state::AppState

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use crate::store::{
    self, ChatAttachmentFull, ChatAttachmentMeta, ChatContext, ChatMessageRow, ChatMessageUpdate,
    ChatReply, ChatThread, ContinueSave, DeleteAttachmentOutcome, KeptAttachment, NewAttachment,
    SendMessageOutcome, SetModeOutcome,
};

/// Every temporary thread of this gateway. `state.chat_temp`.
#[derive(Default)]
pub struct TempChats {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The id handed out last; the next one is one below it. `0` before the
    /// first, so the first id is −1.
    last_id: i64,
    threads: BTreeMap<i64, TempThread>,
    /// Threads being kept right now: out of `threads` while their DB insert
    /// runs, so a second Keep is refused rather than storing them twice
    /// (review R1 finding 8).
    keeping: HashSet<i64>,
}

/// What [`TempChats::take_for_keep`] found.
pub(crate) enum TakeForKeep {
    /// The thread, now out of the map until [`TempChats::keep_done`] or
    /// [`TempChats::keep_failed`].
    Taken(Box<TempThread>),
    /// Another Keep of it is running.
    Busy,
    NotFound,
}

impl Inner {
    fn next_id(&mut self) -> i64 {
        self.last_id -= 1;
        self.last_id
    }
}

/// One temporary thread, whole.
#[derive(Debug, Clone)]
pub(crate) struct TempThread {
    pub thread: ChatThread,
    /// In conversation order.
    pub messages: Vec<ChatMessageRow>,
    /// Drafts and sent ones, in upload order.
    pub attachments: Vec<TempAttachment>,
}

impl TempThread {
    fn touch(&mut self) {
        self.thread.updated_at = now();
    }

    fn position(&self, message_id: i64) -> Option<usize> {
        self.messages.iter().position(|m| m.id == message_id)
    }

    /// Drop messages `from..`, and their attachments.
    fn cut(&mut self, from: usize) -> u64 {
        let gone: Vec<i64> = self.messages.drain(from..).map(|m| m.id).collect();
        self.attachments
            .retain(|a| !a.message_id.is_some_and(|m| gone.contains(&m)));
        gone.len() as u64
    }
}

/// A temporary thread's attachment: the DB row's columns.
#[derive(Debug, Clone)]
pub(crate) struct TempAttachment {
    pub id: i64,
    pub message_id: Option<i64>,
    pub kind: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub data: Vec<u8>,
    pub ord: i64,
    pub created_at: String,
    pub extracted: Option<String>,
    pub meta: serde_json::Value,
    pub mode: Option<String>,
    /// Rendered PDF pages, by 1-based page number.
    pub pages: HashMap<u32, Vec<u8>>,
}

impl TempAttachment {
    fn meta(&self, thread_id: i64) -> ChatAttachmentMeta {
        ChatAttachmentMeta {
            id: self.id,
            thread_id,
            message_id: self.message_id,
            kind: self.kind.clone(),
            name: self.name.clone(),
            mime: self.mime.clone(),
            size: self.size,
            mode: self.mode.clone(),
            extracted_tokens: store::attachment_tokens(&self.meta),
            meta: self.meta.clone(),
            blockers: None,
        }
    }

    fn full(&self, thread_id: i64, with_data: bool) -> ChatAttachmentFull {
        ChatAttachmentFull {
            id: self.id,
            thread_id,
            message_id: self.message_id,
            kind: self.kind.clone(),
            name: self.name.clone(),
            mime: self.mime.clone(),
            data: if with_data {
                self.data.clone()
            } else {
                Vec::new()
            },
            extracted: self.extracted.clone(),
            meta: self.meta.clone(),
            mode: self.mode.clone(),
        }
    }

    /// For Keep: the row as the DB insert takes it.
    pub fn kept(&self) -> KeptAttachment {
        KeptAttachment {
            message_id: self.message_id,
            kind: self.kind.clone(),
            name: self.name.clone(),
            mime: self.mime.clone(),
            size: self.size,
            ord: self.ord,
            data: self.data.clone(),
            created_at: self.created_at.clone(),
            extracted: self.extracted.clone(),
            meta: self.meta.clone(),
            mode: self.mode.clone(),
            pages: self.pages.iter().map(|(p, b)| (*p, b.clone())).collect(),
        }
    }
}

/// `datetime('now')`'s shape, so a temporary thread's timestamps read like a
/// stored one's — and stay valid when Keep writes them to the DB.
fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

impl TempChats {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Nothing here panics while holding the lock; a poisoned one still
        // holds consistent maps, so carry on rather than take the Chat down.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_thread<T>(&self, id: i64, f: impl FnOnce(&mut TempThread) -> T) -> Option<T> {
        self.lock().threads.get_mut(&id).map(f)
    }

    /// A new temporary thread, returned as created.
    pub fn create(&self, model_alias: &str, kind: &str, system_prompt: &str) -> ChatThread {
        let mut inner = self.lock();
        let id = inner.next_id();
        let at = now();
        let thread = ChatThread {
            id,
            title: "New chat".to_string(),
            model_alias: model_alias.to_string(),
            system_prompt: system_prompt.to_string(),
            kind: kind.to_string(),
            created_at: at.clone(),
            updated_at: at,
            ..Default::default()
        };
        inner.threads.insert(
            id,
            TempThread {
                thread: thread.clone(),
                messages: Vec::new(),
                attachments: Vec::new(),
            },
        );
        thread
    }

    /// Every temporary thread, most recently active first.
    pub fn list(&self) -> Vec<ChatThread> {
        let mut out: Vec<ChatThread> = self
            .lock()
            .threads
            .values()
            .map(|t| t.thread.clone())
            .collect();
        // Newer ids are more negative: ascending id is newest first.
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        out
    }

    pub fn thread(&self, id: i64) -> Option<ChatThread> {
        self.lock().threads.get(&id).map(|t| t.thread.clone())
    }

    /// Take the whole thread out for Keep: it leaves the map (a Keep running
    /// alongside finds it busy), until Keep reports back.
    pub(crate) fn take_for_keep(&self, id: i64) -> TakeForKeep {
        let mut inner = self.lock();
        if inner.keeping.contains(&id) {
            return TakeForKeep::Busy;
        }
        match inner.threads.remove(&id) {
            Some(t) => {
                inner.keeping.insert(id);
                TakeForKeep::Taken(Box::new(t))
            }
            None => TakeForKeep::NotFound,
        }
    }

    /// Keep wrote the thread to the DB: it is gone from here for good.
    pub(crate) fn keep_done(&self, id: i64) {
        self.lock().keeping.remove(&id);
    }

    /// Keep did not write it: the thread goes back, as it was.
    pub(crate) fn keep_failed(&self, t: TempThread) {
        let mut inner = self.lock();
        inner.keeping.remove(&t.thread.id);
        inner.threads.insert(t.thread.id, t);
    }

    /// Write `t`'s settings over the thread `t.id` — every editable field,
    /// as `store::update_chat_thread_settings` does; the id, the creation
    /// time and the flags a temporary thread never has stay.
    pub fn update_settings(&self, t: &ChatThread) -> bool {
        self.with_thread(t.id, |tt| {
            let keep_created = std::mem::take(&mut tt.thread.created_at);
            tt.thread = ChatThread {
                created_at: keep_created,
                pinned: false,
                archived_at: None,
                ..t.clone()
            };
            tt.touch();
        })
        .is_some()
    }

    /// Set just the title — no `updated_at` bump, like the DB's.
    pub fn set_title(&self, id: i64, title: &str) {
        self.with_thread(id, |t| t.thread.title = title.to_string());
    }

    /// Discard a thread with everything in it.
    pub fn delete(&self, id: i64) -> bool {
        self.lock().threads.remove(&id).is_some()
    }

    pub fn messages(&self, thread_id: i64) -> Vec<ChatMessageRow> {
        self.with_thread(thread_id, |t| t.messages.clone())
            .unwrap_or_default()
    }

    pub fn message(&self, thread_id: i64, id: i64) -> Option<ChatMessageRow> {
        self.with_thread(thread_id, |t| {
            t.messages.iter().find(|m| m.id == id).cloned()
        })
        .flatten()
    }

    pub fn last_message(&self, thread_id: i64) -> Option<ChatMessageRow> {
        self.with_thread(thread_id, |t| t.messages.last().cloned())
            .flatten()
    }

    /// Append a message; `None` when the thread is gone (discarded or kept
    /// while a reply was still streaming into it).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn append_message(
        &self,
        thread_id: i64,
        role: &str,
        content: &str,
        reasoning: &str,
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
        ir_messages: Option<&str>,
    ) -> Option<i64> {
        let mut inner = self.lock();
        let id = inner.next_id();
        let t = inner.threads.get_mut(&thread_id)?;
        t.messages.push(ChatMessageRow {
            id,
            thread_id,
            role: role.to_string(),
            content: content.to_string(),
            reasoning: reasoning.to_string(),
            prompt_tokens,
            completion_tokens,
            ir_messages: ir_messages.map(str::to_string),
            created_at: now(),
            ..Default::default()
        });
        t.touch();
        Some(id)
    }

    /// Append a turn's reply; `None` when the thread is gone.
    pub fn append_reply(&self, thread_id: i64, r: &ChatReply) -> Option<i64> {
        let mut inner = self.lock();
        let id = inner.next_id();
        let t = inner.threads.get_mut(&thread_id)?;
        t.messages.push(ChatMessageRow {
            id,
            thread_id,
            role: "assistant".to_string(),
            content: r.content.clone(),
            reasoning: r.reasoning.clone(),
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            ir_messages: r.ir_messages.clone(),
            model: r.model.clone(),
            answered_by: r.answered_by.clone(),
            created_at: now(),
            ..Default::default()
        });
        t.touch();
        Some(id)
    }

    /// Save a continue onto reply `id` while its text is still `prefix` —
    /// `store::continue_chat_reply`'s rule.
    pub fn continue_reply(
        &self,
        thread_id: i64,
        id: i64,
        prefix: &str,
        r: &ChatReply,
    ) -> ContinueSave {
        self.with_thread(thread_id, |t| {
            let Some(row) = t
                .messages
                .iter_mut()
                .find(|m| m.id == id && m.role == "assistant")
            else {
                return ContinueSave::Gone;
            };
            if row.content.trim_end() != prefix {
                return ContinueSave::Changed;
            }
            row.content = r.content.clone();
            row.reasoning = r.reasoning.clone();
            row.prompt_tokens = r.prompt_tokens;
            row.completion_tokens = r.completion_tokens;
            row.ir_messages = r.ir_messages.clone();
            row.model = r.model.clone();
            row.answered_by = r.answered_by.clone();
            t.touch();
            ContinueSave::Saved
        })
        .unwrap_or(ContinueSave::Gone)
    }

    /// Rewrite user message `id` for a resend, all at once —
    /// `store::rewrite_chat_user_message`'s rule. `false` when it is not a
    /// user message of this thread.
    pub fn rewrite_user_message(
        &self,
        thread_id: i64,
        id: i64,
        content: &str,
        kb_refs: &[i64],
    ) -> bool {
        self.with_thread(thread_id, |t| {
            let Some(pos) = t
                .messages
                .iter()
                .position(|m| m.id == id && m.role == "user")
            else {
                return false;
            };
            let row = &mut t.messages[pos];
            row.content = content.to_string();
            row.kb_refs = kb_refs.to_vec();
            row.context = None;
            t.cut(pos + 1);
            t.touch();
            true
        })
        .unwrap_or(false)
    }

    /// A user message with its drafts bound to it, all or nothing, and the
    /// knowledge bases it names for itself — the in-memory
    /// `store::append_user_message_with_kb_refs`. `None` when the thread is
    /// gone.
    pub fn append_user_message(
        &self,
        thread_id: i64,
        content: &str,
        attachment_ids: &[i64],
        kb_refs: &[i64],
    ) -> Option<SendMessageOutcome> {
        let mut inner = self.lock();
        let id = inner.next_id();
        let t = inner.threads.get_mut(&thread_id)?;
        // Checked first, bound after: one lock covers both, so there is no
        // concurrent send to lose a draft to — but a duplicate or a sent id is
        // still refused, as the DB refuses it.
        let mut seen = Vec::with_capacity(attachment_ids.len());
        for aid in attachment_ids {
            let draft = t
                .attachments
                .iter()
                .any(|a| a.id == *aid && a.message_id.is_none());
            if !draft || seen.contains(aid) {
                return Some(SendMessageOutcome::AttachmentNotDraft);
            }
            seen.push(*aid);
        }
        for (ord, aid) in attachment_ids.iter().enumerate() {
            if let Some(a) = t.attachments.iter_mut().find(|a| a.id == *aid) {
                a.message_id = Some(id);
                a.ord = ord as i64;
            }
        }
        t.messages.push(ChatMessageRow {
            id,
            thread_id,
            role: "user".to_string(),
            content: content.to_string(),
            kb_refs: kb_refs.to_vec(),
            created_at: now(),
            ..Default::default()
        });
        t.touch();
        Some(SendMessageOutcome::Sent(id))
    }

    pub fn update_message(&self, thread_id: i64, id: i64, m: &ChatMessageUpdate) -> bool {
        self.with_thread(thread_id, |t| {
            let Some(row) = t.messages.iter_mut().find(|r| r.id == id) else {
                return false;
            };
            row.content = m.content.clone();
            row.reasoning = m.reasoning.clone();
            row.prompt_tokens = m.prompt_tokens;
            row.completion_tokens = m.completion_tokens;
            row.ir_messages = m.ir_messages.clone();
            t.touch();
            true
        })
        .unwrap_or(false)
    }

    /// Set a message's `kb_refs` and `context` together, as
    /// `store::set_chat_message_knowledge` does — not activity, so no
    /// `updated_at` bump.
    pub fn set_message_knowledge(
        &self,
        thread_id: i64,
        id: i64,
        kb_refs: &[i64],
        context: Option<&ChatContext>,
    ) -> bool {
        self.with_thread(thread_id, |t| {
            let Some(row) = t.messages.iter_mut().find(|r| r.id == id) else {
                return false;
            };
            row.kb_refs = kb_refs.to_vec();
            row.context = context.cloned();
            true
        })
        .unwrap_or(false)
    }

    pub fn delete_message(&self, thread_id: i64, id: i64) -> bool {
        self.with_thread(thread_id, |t| {
            let Some(pos) = t.position(id) else {
                return false;
            };
            t.messages.remove(pos);
            t.attachments.retain(|a| a.message_id != Some(id));
            t.touch();
            true
        })
        .unwrap_or(false)
    }

    /// Cut back to message `id` (dropping it too when `inclusive`); how many
    /// messages went — `0` also when `id` is not in the thread.
    pub fn truncate(&self, thread_id: i64, id: i64, inclusive: bool) -> u64 {
        self.with_thread(thread_id, |t| {
            let Some(pos) = t.position(id) else {
                return 0;
            };
            let n = t.cut(if inclusive { pos } else { pos + 1 });
            if n > 0 {
                t.touch();
            }
            n
        })
        .unwrap_or(0)
    }

    /// Store an upload as a draft; `None` when the thread is gone.
    pub fn insert_attachment(&self, thread_id: i64, a: &NewAttachment) -> Option<i64> {
        let mut inner = self.lock();
        let id = inner.next_id();
        let t = inner.threads.get_mut(&thread_id)?;
        t.attachments.push(TempAttachment {
            id,
            message_id: None,
            kind: a.kind.clone(),
            name: a.name.clone(),
            mime: a.mime.clone(),
            size: a.data.len() as i64,
            data: a.data.clone(),
            ord: 0,
            created_at: now(),
            extracted: a.extracted.clone(),
            meta: a.meta.clone(),
            mode: a.mode.clone(),
            pages: HashMap::new(),
        });
        Some(id)
    }

    fn with_attachment<R>(&self, id: i64, f: impl FnOnce(&mut TempAttachment) -> R) -> Option<R> {
        let mut inner = self.lock();
        inner
            .threads
            .values_mut()
            .find_map(|t| t.attachments.iter_mut().find(|a| a.id == id))
            .map(f)
    }

    /// Store text derived after the upload, with its metadata.
    pub fn set_extracted(&self, id: i64, text: &str, meta: &serde_json::Value) {
        self.with_attachment(id, |a| {
            a.extracted = Some(text.to_string());
            a.meta = meta.clone();
        });
    }

    /// A text-class PDF's mode, under the same rules as the DB's.
    pub fn set_mode(&self, id: i64, mode: &str) -> SetModeOutcome {
        self.with_attachment(id, |a| {
            if a.message_id.is_some() {
                SetModeOutcome::Sent
            } else if !store::is_text_pdf(&a.kind, &a.meta) {
                SetModeOutcome::NotTextPdf
            } else {
                a.mode = Some(mode.to_string());
                SetModeOutcome::Set
            }
        })
        .unwrap_or(SetModeOutcome::NotFound)
    }

    pub fn page(&self, id: i64, page: u32) -> Option<Vec<u8>> {
        self.with_attachment(id, |a| a.pages.get(&page).cloned())
            .flatten()
    }

    pub fn put_page(&self, id: i64, page: u32, png: &[u8]) {
        self.with_attachment(id, |a| {
            a.pages.insert(page, png.to_vec());
        });
    }

    /// Every attachment's metadata, each message's in the order it sent them.
    pub fn attachments_meta(&self, thread_id: i64) -> Vec<ChatAttachmentMeta> {
        self.with_thread(thread_id, |t| {
            let mut v: Vec<&TempAttachment> = t.attachments.iter().collect();
            v.sort_by_key(|a| a.ord);
            v.into_iter().map(|a| a.meta(thread_id)).collect()
        })
        .unwrap_or_default()
    }

    /// Each attachment's `(ord, created_at)`, by id —
    /// `store::chat_attachment_order`.
    pub fn attachment_order(&self, thread_id: i64) -> HashMap<i64, (i64, String)> {
        self.with_thread(thread_id, |t| {
            t.attachments
                .iter()
                .map(|a| (a.id, (a.ord, a.created_at.clone())))
                .collect()
        })
        .unwrap_or_default()
    }

    /// Every sent attachment with its bytes — an image's left out when
    /// `vision` already says the model cannot see it, as the DB query does.
    pub fn sent_attachments(
        &self,
        thread_id: i64,
        vision: Option<bool>,
    ) -> Vec<ChatAttachmentFull> {
        let skip_images = vision == Some(false);
        self.with_thread(thread_id, |t| {
            let mut v: Vec<&TempAttachment> = t
                .attachments
                .iter()
                .filter(|a| a.message_id.is_some())
                .collect();
            v.sort_by_key(|a| a.ord);
            v.into_iter()
                .map(|a| a.full(thread_id, !(skip_images && a.kind == "image")))
                .collect()
        })
        .unwrap_or_default()
    }

    /// The drafts among `ids`, in upload order.
    pub fn drafts_by_ids(&self, thread_id: i64, ids: &[i64]) -> Vec<ChatAttachmentMeta> {
        self.with_thread(thread_id, |t| {
            t.attachments
                .iter()
                .filter(|a| a.message_id.is_none() && ids.contains(&a.id))
                .map(|a| a.meta(thread_id))
                .collect()
        })
        .unwrap_or_default()
    }

    /// One attachment with its bytes, from whichever thread holds it.
    pub fn attachment(&self, id: i64) -> Option<ChatAttachmentFull> {
        let inner = self.lock();
        inner.threads.values().find_map(|t| {
            t.attachments
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.full(t.thread.id, true))
        })
    }

    /// Delete a draft; a sent one is refused, as the DB refuses it.
    pub fn delete_draft(&self, id: i64) -> DeleteAttachmentOutcome {
        let mut inner = self.lock();
        for t in inner.threads.values_mut() {
            if let Some(pos) = t.attachments.iter().position(|a| a.id == id) {
                if t.attachments[pos].message_id.is_some() {
                    return DeleteAttachmentOutcome::AlreadySent;
                }
                t.attachments.remove(pos);
                return DeleteAttachmentOutcome::Deleted;
            }
        }
        DeleteAttachmentOutcome::NotFound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_come_from_one_counter_going_down() {
        let c = TempChats::default();
        let t = c.create("m", "chat", "");
        assert_eq!(t.id, -1);
        let a = c.insert_attachment(
            t.id,
            &NewAttachment::plain("text", "a.txt", "text/plain", b"x"),
        );
        assert_eq!(a, Some(-2));
        let m = c.append_message(t.id, "user", "hi", "", None, None, None);
        assert_eq!(m, Some(-3));
        assert_eq!(c.create("m", "chat", "").id, -4);
    }

    #[test]
    fn truncating_keeps_order_and_drops_attachments_of_what_went() {
        let c = TempChats::default();
        let t = c.create("m", "chat", "").id;
        let a1 = c
            .insert_attachment(t, &NewAttachment::plain("text", "1", "text/plain", b"1"))
            .unwrap();
        let u1 = match c.append_user_message(t, "one", &[a1], &[]).unwrap() {
            SendMessageOutcome::Sent(id) => id,
            SendMessageOutcome::AttachmentNotDraft => panic!("a1 is a draft"),
        };
        let r1 = c
            .append_message(t, "assistant", "r1", "", None, None, None)
            .unwrap();
        let a2 = c
            .insert_attachment(t, &NewAttachment::plain("text", "2", "text/plain", b"2"))
            .unwrap();
        let SendMessageOutcome::Sent(u2) = c.append_user_message(t, "two", &[a2], &[]).unwrap()
        else {
            panic!("a2 is a draft")
        };
        c.append_message(t, "assistant", "r2", "", None, None, None);

        assert_eq!(c.truncate(t, r1, false), 2);
        let ids: Vec<i64> = c.messages(t).iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![u1, r1]);
        assert!(c.attachment(a2).is_none(), "u2's file went with it");
        assert!(c.attachment(a1).is_some());
        assert!(c.message(t, u2).is_none());

        assert_eq!(c.truncate(t, u1, true), 2);
        assert!(c.messages(t).is_empty());
        assert!(c.attachment(a1).is_none());
    }

    #[test]
    fn a_draft_binds_once_and_a_sent_one_cannot_be_deleted() {
        let c = TempChats::default();
        let t = c.create("m", "chat", "").id;
        let a = c
            .insert_attachment(t, &NewAttachment::plain("text", "f", "text/plain", b"f"))
            .unwrap();
        assert!(matches!(
            c.append_user_message(t, "x", &[a, a], &[]),
            Some(SendMessageOutcome::AttachmentNotDraft)
        ));
        assert!(c.messages(t).is_empty(), "a refused send writes nothing");
        assert!(matches!(
            c.append_user_message(t, "x", &[a], &[]),
            Some(SendMessageOutcome::Sent(_))
        ));
        assert!(matches!(
            c.append_user_message(t, "y", &[a], &[]),
            Some(SendMessageOutcome::AttachmentNotDraft)
        ));
        assert!(matches!(
            c.delete_draft(a),
            DeleteAttachmentOutcome::AlreadySent
        ));
        assert!(matches!(
            c.delete_draft(-99),
            DeleteAttachmentOutcome::NotFound
        ));
    }

    #[test]
    fn a_message_of_another_thread_is_not_found() {
        let c = TempChats::default();
        let t1 = c.create("m", "chat", "").id;
        let t2 = c.create("m", "chat", "").id;
        let m = c
            .append_message(t1, "user", "hi", "", None, None, None)
            .unwrap();
        assert!(c.message(t2, m).is_none());
        assert!(!c.delete_message(t2, m));
        assert_eq!(c.truncate(t2, m, true), 0);
        assert!(c.message(t1, m).is_some());
    }
}
