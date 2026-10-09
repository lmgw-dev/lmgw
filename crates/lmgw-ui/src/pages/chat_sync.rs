//! The Chat page follows what other writers change (client-apps design
//! §3.6, 2026-10-08): a paired device, an owner key, an MCP tool, another
//! dashboard window, the sweep. `/api/events`' `chat` frame names the
//! threads, folders and messages that changed ([`crate::live::ChatInbox`]);
//! the page reads those again through the Chat API, as it reads them when
//! it opens. A frame never carries what to show.
//!
//! - **The list** — threads, folders and their current-thread marks, the
//!   archived count — is read again on a frame that names a thread or a
//!   folder, or says resync. [`ListRead`] is the one read the page's own
//!   refresh uses too: the newest read wins, and a read that changes
//!   nothing sets nothing.
//! - **A frame that names only messages** moved those threads' times and
//!   nothing else the list shows: only their rows are read
//!   (`GET /chat/api/threads/rows`), and the list is sorted again here as
//!   the server sorts it ([`rows`], review CL-11). A named row that is gone,
//!   or a read of them that failed, reads the whole list instead.
//! - **The open thread** is read again when a frame names it (its row or
//!   its messages) or says resync: its row takes the stored one
//!   ([`rules::row_take`]), its messages take the stored rows in place
//!   (`plan`), so what did not change is not re-mounted. A reader who was
//!   at the end of the transcript before the rows landed is kept there,
//!   one who scrolled up is left where they are.
//! - **Newest read wins here too** ([`OwnEdits`]): a read that went out
//!   before the page's own turn, edit or delete changed the transcript and
//!   answers after it is dropped and made again, never applied.
//! - **What the page is doing comes first.** While the page's own reply
//!   streams into the open thread, or a live voice session holds the
//!   transcript, the read waits and runs once that ends; the composer, its
//!   draft files and the thread settings being edited are never touched (an
//!   unedited settings form follows the stored settings, an edited one
//!   follows in the fields the owner did not touch). The page's own writes
//!   come back as frames too, and read back as what the page already shows.
//! - **A thread deleted elsewhere** (a 404, and only a 404) is the page's
//!   to leave (`Follow::gone`): a draft in the composer waits for a new
//!   chat that is made only when the owner presses Send (`gone`), never
//!   going into another conversation. Any other failed read keeps what the
//!   page shows, says so once, and the next frame reads again.
//! - **Coalesced**: one read runs at a time; frames that come meanwhile are
//!   merged and read once after it.
//!
//! A reply another client is generating shows when it is saved (the feed's
//! live turns are not mirrored, §2.5); its user turn shows as soon as it is
//! stored.

mod gone;
mod own_delete;
mod plan;
mod reads;
mod rows;
mod rules;

use leptos::prelude::*;
use lmgw_api_types::ChatChanged;

use super::chat::{
    at_end, scroll_down, ChatThread, Msg, MsgRow, SettingsDraft, ThreadDetail, ThreadsResponse,
};
use super::chat_attach::{fold_drafts, DraftChip};
use super::chat_folders::FolderInfo;
use super::chat_rows::{appended, patch};
use crate::scope::{Latest, Scope};
use crate::widgets::Toasts;
pub(super) use gone::{
    held_model, leave as leave_new_chat, rescued_open, restore as restore_new_chat, send_new,
    watch as watch_new_chat, Gone, Landing, NewChatStrip, Opened, Rescue, SendNew, HELD,
    NO_FILES_YET, OPENING, WAITS_AGAIN,
};
use own_delete::gone_elsewhere;
pub(super) use own_delete::own_folder_delete;
use plan::{plan, Plan, Shown};
use reads::{asked, Asked};
pub(super) use rules::ModelPicks;
use rules::{landed, row_take, Landed, RowTake};

/// The thread list's signals and its one read.
#[derive(Clone, Copy)]
pub(super) struct ListRead {
    pub threads: RwSignal<Vec<ChatThread>>,
    pub temporary: RwSignal<Vec<ChatThread>>,
    pub folders: RwSignal<Vec<FolderInfo>>,
    pub archived_count: RwSignal<i64>,
    /// The read's state: `None` while the first is out, the error of the
    /// last one that failed.
    pub state: RwSignal<Option<Result<(), String>>>,
    /// Which list the toolbar shows.
    pub view_archived: RwSignal<bool>,
    pub latest: Latest,
    /// Reads of the whole list out now ([`Self::fetch`]): a read of some
    /// rows is made only while none is, since its ticket would drop that
    /// read's answer and the changes it brings beyond those rows.
    pub out: StoredValue<u32>,
}

impl ListRead {
    /// The URL of the list the toolbar shows.
    pub(super) fn url(self) -> &'static str {
        if self.view_archived.get_untracked() {
            "/chat/api/threads?archived=1"
        } else {
            "/chat/api/threads"
        }
    }

    /// The whole list as the server answers it now, under its ticket: every
    /// read of the whole list goes through this, so the ones out are
    /// counted. `None` once the page is gone.
    pub(super) async fn fetch(self) -> Option<(u64, Result<ThreadsResponse, crate::api::Error>)> {
        let ticket = self.latest.next()?;
        self.out.try_update_value(|n| *n += 1);
        let got = crate::api::get::<ThreadsResponse>(self.url()).await;
        self.out.try_update_value(|n| *n = n.saturating_sub(1));
        Some((ticket, got))
    }

    /// Read the list again. Only the newest read lands, and only what
    /// differs is set; a failed read keeps the list and says so above it.
    pub(super) async fn read(self) {
        let Some((ticket, got)) = self.fetch().await else {
            return;
        };
        if !self.latest.is(ticket) {
            return;
        }
        match got {
            Ok(r) => self.take(r),
            Err(e) => self.state.set(Some(Err(e.to_string()))),
        }
    }

    /// The list as read, set where it differs.
    pub(super) fn take(self, r: ThreadsResponse) {
        set_if_changed(self.threads, r.threads);
        set_if_changed(self.temporary, r.temporary);
        set_if_changed(self.folders, r.folders);
        set_if_changed(self.archived_count, r.archived_count);
        if !matches!(self.state.get_untracked(), Some(Ok(()))) {
            self.state.set(Some(Ok(())));
        }
    }
}

fn set_if_changed<T: PartialEq + Send + Sync + 'static>(s: RwSignal<T>, v: T) {
    if s.with_untracked(|cur| *cur != v) {
        s.set(v);
    }
}

/// The page's own changes to the open transcript, counted (review CL-3):
/// a turn as it starts and as it ends, a message deleted or edited as the
/// write goes out and as it lands, a thread opened. A read of the open
/// thread that went out before one and answers after it may be older than
/// what the page shows, so it is dropped and made again. `Copy`.
#[derive(Clone, Copy)]
pub(super) struct OwnEdits(StoredValue<u64>);

impl OwnEdits {
    #[allow(clippy::new_without_default)] // made where its owner is; not a value
    pub(super) fn new() -> Self {
        Self(StoredValue::new(0))
    }

    /// The page changed, or is about to change, the open transcript.
    pub(super) fn bump(self) {
        self.0.try_update_value(|n| *n += 1);
    }

    /// The count now; `None` once the page is gone.
    fn now(self) -> Option<u64> {
        self.0.try_get_value()
    }
}

/// What the page hands the follower. `Copy`.
#[derive(Clone, Copy)]
pub(super) struct Follow {
    pub list: ListRead,
    pub scope: Scope,
    pub current: RwSignal<Option<ChatThread>>,
    pub current_id: Memo<Option<i64>>,
    pub msgs: RwSignal<Vec<Msg>>,
    /// The thread the page's own reply streams into.
    pub streaming: RwSignal<Option<i64>>,
    /// A voice session holds the transcript: live, or one closing.
    pub voice_busy: Signal<bool>,
    pub own: OwnEdits,
    pub settings: SettingsDraft,
    pub settings_unsaved: Memo<bool>,
    pub model_sel: RwSignal<String>,
    /// The picker's picks on their way; a model followed from the stored
    /// row is marked there, so it is never saved back.
    pub picks: StoredValue<ModelPicks>,
    /// The composer's chips: their blockers follow the stored model.
    pub drafts: RwSignal<Vec<DraftChip>>,
    /// The open thread's MCP tasks (`chat_tasks`): read with it.
    pub tasks: super::chat_tasks::Tasks,
    /// Messages for rows, owned by the page.
    pub make: Callback<Vec<MsgRow>, Vec<Msg>>,
    /// Show these rows as the open thread's messages, afresh.
    pub load: Callback<Vec<MsgRow>>,
    /// The open thread is gone, deleted elsewhere: the page leaves it.
    pub gone: Callback<i64>,
    /// A delete of this thread is the page's own, under way.
    pub deleting: StoredValue<Option<i64>>,
    pub toasts: Toasts,
}

/// What is to be read, and whether a read runs.
#[derive(Default)]
struct Wants {
    list: bool,
    /// The rows of these threads, when the whole list is not wanted.
    rows: std::collections::BTreeSet<i64>,
    /// The open thread, when it was named.
    open: Option<i64>,
    /// A read of the open thread waits for the page (module doc).
    deferred: bool,
    running: bool,
    /// The last read of the open thread failed, and said so: the next
    /// failure is only logged.
    failing: bool,
}

/// Follow the `chat` frames for the page's lifetime.
pub(super) fn follow(f: Follow) {
    let inbox = crate::live::use_live().chat;
    let wants = StoredValue::new(Wants::default());
    // What came before the page: it reads everything as it opens.
    inbox.take();
    Effect::new(move |_| {
        inbox.arrived();
        let c = inbox.take();
        if c.is_empty() {
            return;
        }
        want(f, wants, &c);
        kick(f, wants);
    });
    // The page's own stream or voice session ended: a read that waited for
    // it runs now. A session going live or ending changes the transcript as
    // the page's own work does.
    Effect::new(move |prev: Option<bool>| {
        let voice = f.voice_busy.get();
        if prev.is_some_and(|p| p != voice) {
            f.own.bump();
        }
        let open = f.current_id.get();
        let busy = (open.is_some() && f.streaming.get() == open) || voice;
        if busy {
            return voice;
        }
        let waited = wants
            .try_update_value(|w| {
                let d = std::mem::take(&mut w.deferred);
                if d {
                    w.open = open;
                }
                d
            })
            .unwrap_or(false);
        if waited {
            kick(f, wants);
        }
        voice
    });
}

/// Note what frame `c` asks to be read ([`asked`]): only messages read
/// their threads' rows, not the whole list (module doc).
fn want(f: Follow, wants: StoredValue<Wants>, c: &ChatChanged) {
    let Asked { list, rows, open } = asked(c, f.current_id.get_untracked());
    wants.update_value(|w| {
        w.list |= list;
        w.rows.extend(rows);
        if open.is_some() {
            w.open = open;
        }
    });
}

/// Run the reads, one at a time, until nothing is wanted.
fn kick(f: Follow, wants: StoredValue<Wants>) {
    let start = wants
        .try_update_value(|w| !std::mem::replace(&mut w.running, true))
        .unwrap_or(false);
    if !start {
        return;
    }
    f.scope.spawn(async move {
        loop {
            let Some((list, rows, open)) = wants.try_update_value(|w| {
                (
                    std::mem::take(&mut w.list),
                    std::mem::take(&mut w.rows),
                    w.open.take(),
                )
            }) else {
                return;
            };
            if !list && rows.is_empty() && open.is_none() {
                break;
            }
            if list {
                f.list.read().await;
            } else if !rows.is_empty() {
                rows::read(f.list, rows.into_iter().collect()).await;
            }
            if let Some(id) = open {
                read_open(f, wants, id).await;
            }
        }
        wants.update_value(|w| w.running = false);
    });
}

/// The page's own work holds thread `id`'s transcript.
fn busy(f: Follow, id: i64) -> bool {
    f.streaming.get_untracked() == Some(id) || f.voice_busy.get_untracked()
}

/// Read the open thread `id` again and show it as stored.
async fn read_open(f: Follow, wants: StoredValue<Wants>, id: i64) {
    if f.current_id.get_untracked() != Some(id) {
        return;
    }
    if busy(f, id) {
        wants.update_value(|w| w.deferred = true);
        return;
    }
    let Some(sent_at) = f.own.now() else {
        return;
    };
    let got = crate::api::get::<ThreadDetail>(format!("/chat/api/threads/{id}")).await;
    let now = landed(
        id,
        f.current_id.get_untracked(),
        busy(f, id),
        sent_at,
        f.own.now(),
    );
    if now == Landed::Left {
        return;
    }
    let d = match got {
        Ok(d) => d,
        // Gone: a 404 and only a 404 (review CL-4).
        Err(e) if e.is_not_found() => {
            if gone_elsewhere(id, f.deleting.get_value()) {
                f.gone.run(id);
            }
            return;
        }
        // What the page shows stays; the next frame reads it again.
        Err(e) => {
            leptos::logging::warn!("reading the open conversation again failed: {e}");
            let first = wants
                .try_update_value(|w| !std::mem::replace(&mut w.failing, true))
                .unwrap_or(false);
            if first {
                f.toasts.warn(format!(
                    "reading the open conversation again failed: {e} — it shows what it \
                     showed, and is read again on the next change"
                ));
            }
            return;
        }
    };
    match now {
        Landed::Left | Landed::Take => {}
        Landed::Wait => {
            wants.update_value(|w| w.deferred = true);
            return;
        }
        // The page changed the transcript while this read was out: the
        // answer may be older than what it shows. Read again.
        Landed::Again => {
            wants.update_value(|w| w.open = Some(id));
            return;
        }
    }
    wants.update_value(|w| w.failing = false);
    take_thread(f, d.thread);
    fold_drafts(f.drafts, &d.draft_attachments);
    super::chat_tasks::take(f.tasks, id, d.tasks);
    take_messages(f, d.messages);
}

/// The open thread's row as stored ([`rules::row_take`]). An unedited
/// settings form follows it, an edited one in the fields the owner did not
/// touch; the model picker follows while it shows the model the page held.
fn take_thread(f: Follow, t: ChatThread) {
    // A pick of the owner's on its way settles the picker (review CL-15).
    let picking = f.picks.try_with_value(|p| p.busy(t.id)).unwrap_or(false);
    let Some((take, held)) = f.current.with_untracked(|c| {
        c.as_ref().map(|held| {
            let take = f.model_sel.with_untracked(|picker| {
                row_take(
                    held,
                    &t,
                    f.settings_unsaved.get_untracked(),
                    picker,
                    picking,
                )
            });
            (take, held.clone())
        })
    }) else {
        return;
    };
    match take {
        RowTake::Same => {}
        RowTake::OnlyWhen => f.current.update_untracked(|c| *c = Some(t)),
        RowTake::Changed {
            reseed,
            follow_model,
        } => {
            let model = t.model_alias.clone();
            let tid = t.id;
            f.current.set(Some(t.clone()));
            if reseed {
                f.settings.seed(&t);
            } else {
                f.settings.rebase(&held, &t);
            }
            if follow_model {
                f.picks.update_value(|p| p.follow(tid, &model));
                f.model_sel.set(model);
            }
        }
    }
}

/// The open thread's messages as stored, in place (`plan`).
fn take_messages(f: Follow, rows: Vec<MsgRow>) {
    // Before anything lands (review CL-1): measured after, a reply taller
    // than the "at the end" margin would read as the reader having scrolled
    // up.
    let follow = at_end();
    let Some(shown) = f.msgs.try_with_untracked(|v| {
        v.iter()
            .map(|m| {
                (
                    m.db_id.get_untracked(),
                    m.role.clone(),
                    m.unsaved.get_untracked(),
                )
            })
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    let page: Vec<Shown<'_>> = shown
        .iter()
        .map(|(id, role, unsaved)| Shown {
            id: *id,
            role,
            unsaved: *unsaved,
        })
        .collect();
    let keys: Vec<(i64, &str)> = rows.iter().map(|r| (r.id, r.role.as_str())).collect();
    let (pairs, adopt, drop, insert, append) = match plan(&page, &keys) {
        Plan::Reload => {
            f.load.run(rows);
            if follow {
                scroll_down(true);
            }
            return;
        }
        Plan::Patch {
            pairs,
            adopt,
            drop,
            insert,
            append,
        } => (pairs, adopt, drop, insert, append),
    };
    let mut changed = false;
    f.msgs.with_untracked(|v| {
        for &(pi, ri) in &pairs {
            changed |= patch(&v[pi], &rows, ri, f.make);
        }
        for &(pi, ri) in &adopt {
            v[pi].db_id.set(Some(rows[ri].id));
            changed |= patch(&v[pi], &rows, ri, f.make);
        }
    });
    if !drop.is_empty() || !insert.is_empty() || !append.is_empty() {
        let added = appended(f.make, &rows, &append);
        let ahead: Vec<usize> = insert.iter().map(|&(_, ri)| ri).collect();
        let ahead = insert
            .iter()
            .map(|&(pi, _)| pi)
            .zip(appended(f.make, &rows, &ahead));
        f.msgs.update(|v| {
            let mut ahead = ahead.peekable();
            let shown = std::mem::take(v);
            for (i, m) in shown.into_iter().enumerate() {
                while let Some((_, a)) = ahead.next_if(|(at, _)| *at == i) {
                    v.push(a);
                }
                if !drop.contains(&i) {
                    v.push(m);
                }
            }
            v.extend(ahead.map(|(_, a)| a));
            v.extend(added);
        });
        changed = true;
    }
    if follow && changed {
        scroll_down(true);
    }
}
