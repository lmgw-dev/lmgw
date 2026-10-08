//! Opening a thread (review CF-8): a row's click, a search hit, the cold
//! load's deep link and its Retry, New chat, Keep, the fall-through after a
//! delete, and the rescued draft's made chat (`chat_sync::gone`). What
//! decides whether an open lands is here and in [`order`]; the page shows
//! what landed ([`Landed`]).
//!
//! - **What the owner chose last shows** ([`order`], reviews CL-20, CF-4):
//!   the open after New chat's or Keep's request yields to a choice the
//!   owner made meanwhile, and the fall-through after a delete and the cold
//!   load's open yield to any open started meanwhile or still out.
//! - **Any other open leaves the rescued draft's new chat** as it starts,
//!   and so does a New chat. The rescued draft's own open keeps the
//!   composer's chips and picks, and its Send hears how it landed.
//! - **An open or a New chat that fails gives the new chat back**, when
//!   nothing else is on its way to show (review CF-1): the page is never
//!   left with the composer up and no thread, no strip, and a Send that
//!   refuses.
//! - **A message to reveal** (a search hit, `?m=`) belongs to the open that
//!   reveals it: any other open drops it as it starts (review CF-5), and
//!   one the transcript does not hold is dropped with a note.

mod order;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use super::super::chat_sync::{self, ListRead, Opened, OwnEdits, Rescue};
use super::super::chat_temp;
use super::{scroll_down, sync_url, ChatThread, Msg, Stats, ThreadDetail};
use crate::scope::Scope;
use crate::widgets::Toasts;
use order::Opens;
use order::Since;

/// Which thread a cold load opens: the `?t=` deep link always wins, even
/// into a thread the (active-only) list just loaded does not carry — it may
/// be archived, since a cold load only fetches the active list (review
/// finding #7: this used to open the first active thread and rewrite the
/// URL out from under an archived deep link) — else the first (most recently
/// active) one, the old app's `firstUpdated` rule (chat-app.js:371-375). A
/// deep link naming a thread that turns out not to exist at all still
/// surfaces as the open's own error toast rather than silently landing
/// on some other conversation.
fn seed_thread(param: Option<i64>, threads: &[ChatThread]) -> Option<i64> {
    param.or_else(|| threads.first().map(|t| t.id))
}

/// What a failure's toast adds when it put the rescued draft's new chat
/// back.
fn waits_again(back: bool) -> &'static str {
    if back {
        chat_sync::WAITS_AGAIN
    } else {
        ""
    }
}

/// An open that landed, for the page to show.
pub(in crate::pages) struct Landed {
    pub id: i64,
    pub detail: ThreadDetail,
    /// The temporary chat open as the open started: thrown away now that
    /// another shows.
    pub left: Option<i64>,
    /// The rescued draft's open: the composer's chips and knowledge picks
    /// stay.
    pub keep_draft: bool,
}

/// What the page hands the opener. `Copy`.
#[derive(Clone, Copy)]
pub(in crate::pages) struct OpenEnv {
    pub current: RwSignal<Option<ChatThread>>,
    pub msgs: RwSignal<Vec<Msg>>,
    pub stats: RwSignal<Option<(i64, Stats)>>,
    pub threads: RwSignal<Vec<ChatThread>>,
    pub model_sel: RwSignal<String>,
    /// A search hit's message, revealed once its thread shows.
    pub focus: RwSignal<Option<i64>>,
    pub rescue: RwSignal<Rescue>,
    pub own: OwnEdits,
    pub list: ListRead,
    /// The cold load's `?t=` deep link.
    pub seed: Option<i64>,
    pub scope: Scope,
    pub toasts: Toasts,
    /// Show what an open read: the page's signals.
    pub show: Callback<Landed>,
    /// Read the thread list again.
    pub refresh: Callback<()>,
}

/// The page's opens. `Copy`.
#[derive(Clone, Copy)]
pub(in crate::pages) struct Opener {
    env: OpenEnv,
    opens: StoredValue<Opens>,
    /// The rescued draft's open of its made chat landing
    /// (`chat_sync::rescued_open`), set once the send it ends in exists.
    rescued: StoredValue<Option<Callback<(u64, Opened)>>>,
}

/// What an open is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// A thread to show: a reveal waiting for another open is dropped.
    Show,
    /// A search hit's thread, or the cold load's (`?m=`): the reveal is
    /// this open's.
    Reveal,
    /// The chat a rescued draft's Send made, by the Send's ticket.
    Rescued(u64),
}

impl Opener {
    pub(in crate::pages) fn new(env: OpenEnv) -> Self {
        Self {
            env,
            opens: StoredValue::new(Opens::default()),
            rescued: StoredValue::new(None),
        }
    }

    /// Where the rescued draft's open of its made chat lands.
    pub(in crate::pages) fn rescued_lands(self, cb: Callback<(u64, Opened)>) {
        self.rescued.set_value(Some(cb));
    }

    fn open_id(self) -> Option<i64> {
        self.env
            .current
            .with_untracked(|c| c.as_ref().map(|t| t.id))
    }

    /// The owner chooses what to show (New chat, Keep): the open after an
    /// earlier choice's request yields to it.
    pub(in crate::pages) fn choose(self) -> Since {
        self.opens
            .try_update_value(Opens::choose)
            .unwrap_or_else(Since::gone)
    }

    /// A row or a search hit for thread `id`: a choice unless that thread is
    /// the open one (review CF-10).
    fn choose_row(self, id: i64) {
        let open = self.open_id() == Some(id);
        self.opens.update_value(|o| {
            o.choose_row(open);
        });
    }

    /// Now, for an open the page makes on its own: the fall-through after a
    /// delete, here or elsewhere, and the cold load.
    pub(in crate::pages) fn now(self) -> Since {
        self.opens
            .try_with_value(Opens::now)
            .unwrap_or_else(Since::gone)
    }

    /// Open thread `id`: the owner's choice.
    pub(in crate::pages) fn open(self, id: i64) {
        self.choose_row(id);
        self.open_with(id, Why::Show);
    }

    /// Open the chat the rescued draft's Send `ticket` made, the draft kept.
    /// The Send's ticket says whether it still goes (`chat_sync::gone`).
    pub(in crate::pages) fn open_rescued(self, id: i64, ticket: u64) {
        self.open_with(id, Why::Rescued(ticket));
    }

    /// Open thread `tid` and reveal its message `mid`: a search hit.
    pub(in crate::pages) fn reveal(self, tid: i64, mid: Option<i64>) {
        self.choose_row(tid);
        self.env.focus.set(mid);
        self.open_with(tid, Why::Reveal);
        // Already open: the open returned at once, so settle here.
        if self.open_id() == Some(tid) {
            self.settle_focus();
        }
    }

    /// What the request of the owner's choice `since` made, thread `id`,
    /// opens unless the owner chose something else since (review CF-4).
    /// Whether it opened.
    pub(in crate::pages) fn follow(self, since: Since, id: i64) -> bool {
        let goes = self
            .opens
            .try_with_value(|o| o.follows(since))
            .unwrap_or(false);
        if goes {
            self.open_with(id, Why::Show);
        }
        goes
    }

    fn open_with(self, id: i64, why: Why) {
        let env = self.env;
        let landed = self.rescued;
        let rescued = match why {
            Why::Rescued(t) => Some(t),
            Why::Show | Why::Reveal => None,
        };
        let tell = move |o: Opened| {
            if let (Some(t), Some(cb)) = (rescued, landed.get_value()) {
                cb.run((t, o));
            }
        };
        // A reveal is the open's that set it: any other drops it as it
        // starts, so a reveal whose open was dropped does not wait for the
        // next one to land (review CF-5).
        if why != Why::Reveal && env.focus.with_untracked(Option::is_some) {
            env.focus.set(None);
        }
        // Already open: a no-op rather than a re-fetch that would overwrite
        // `draft_attachments` from the server's (upload-order) view of the
        // thread's drafts — which does not yet know about a chip whose
        // upload is still in flight, so clicking the open row dropped it
        // until the next reopen (review nit). An open of another thread
        // still out is dropped: this one is the newest.
        if self.open_id() == Some(id) {
            self.opens.try_update_value(Opens::stay);
            tell(Opened::Superseded);
            return;
        }
        if rescued.is_none() {
            chat_sync::leave_new_chat(env.rescue);
        }
        let Some(ticket) = self.opens.try_update_value(Opens::start) else {
            return;
        };
        // Leaving a temporary chat throws it away, without a question — that
        // is what it is for. Only once the next thread has loaded: a failed
        // read keeps showing the temporary chat, so it must still exist.
        let left = env
            .current
            .with_untracked(|c| c.as_ref().filter(|t| t.temporary).map(|t| t.id));
        let opens = self.opens;
        env.scope.spawn(async move {
            let got = crate::api::get::<ThreadDetail>(format!("/chat/api/threads/{id}")).await;
            if !opens.try_update_value(|o| o.lands(ticket)).unwrap_or(false) {
                tell(Opened::Superseded);
                return;
            }
            let o = Opened::of(&got);
            match got {
                Ok(detail) => {
                    env.show.run(Landed {
                        id,
                        detail,
                        left,
                        keep_draft: rescued.is_some(),
                    });
                    self.settle_focus();
                    if env.focus.get_untracked().is_none() {
                        scroll_down(true);
                    }
                    tell(o);
                }
                Err(e) => {
                    // A reveal aimed at this thread has nowhere to land; left
                    // set it would skip the scroll of the next open.
                    env.focus.set(None);
                    if rescued.is_some() {
                        tell(o);
                    } else {
                        // The newest open failed: nothing else shows, so a
                        // rescued draft's new chat it left waits again.
                        let back = chat_sync::restore_new_chat(env.rescue);
                        env.toasts.err(format!("{e}{}", waits_again(back)));
                    }
                }
            }
        });
    }

    /// A message to reveal (search hit, `?m=`) that the open transcript does
    /// not hold is dropped with a note, not waited for: a `focus` left set
    /// would skip the scroll-to-end of every later thread open.
    fn settle_focus(self) {
        let env = self.env;
        if let Some(mid) = env.focus.get_untracked() {
            let has = env
                .msgs
                .with_untracked(|v| v.iter().any(|m| m.db_id.get_untracked() == Some(mid)));
            if !has {
                env.focus.set(None);
                env.toasts.warn("that message no longer exists");
            }
        }
    }

    /// A New chat starts: the owner's latest choice, so the rescued draft's
    /// new chat is left now (review CL-20). An open still out is not
    /// dropped: a New chat that fails leaves it to show (review CF-4).
    pub(in crate::pages) fn starting(self) -> Since {
        let since = self.choose();
        chat_sync::leave_new_chat(self.env.rescue);
        since
    }

    /// The request of the choice `since` (a New chat) failed: the rescued
    /// draft's new chat it left waits again, unless something else is on
    /// its way to show (review CF-1). What its toast adds.
    pub(in crate::pages) fn failed(self, since: Since) -> &'static str {
        let ends = self
            .opens
            .try_with_value(|o| o.failed(since))
            .unwrap_or(false);
        waits_again(ends && chat_sync::restore_new_chat(self.env.rescue))
    }

    /// The cold load, or a Retry of it: the list, then the `?t=` deep link's
    /// thread, else the first one ([`seed_thread`]). Not over an open started
    /// while the list was read, or one still out.
    fn first_load(self) {
        let since = self.now();
        let env = self.env;
        env.scope.spawn(async move {
            let Some((ticket, got)) = env.list.fetch().await else {
                return;
            };
            match got {
                Ok(r) => {
                    let open = seed_thread(env.seed, &r.threads);
                    // A newer read landed meanwhile: it is the list.
                    if env.list.latest.is(ticket) {
                        env.list.take(r);
                    }
                    // An open started meanwhile: that one shows.
                    if !self
                        .opens
                        .try_with_value(|o| o.falls_through(since))
                        .unwrap_or(false)
                    {
                        return;
                    }
                    match open {
                        Some(id) => self.open_with(id, Why::Reveal),
                        None => sync_url(None),
                    }
                }
                Err(e) => {
                    if env.list.latest.is(ticket) {
                        env.list.state.set(Some(Err(e.to_string())));
                    }
                }
            }
        });
    }

    /// The page mounts: the cold load.
    pub(in crate::pages) fn mount(self) {
        self.first_load();
    }

    /// Retry: the first read again while nothing is open, else a re-read. A
    /// rescued draft's new chat counts as open: the list is read again, and
    /// the draft goes on waiting (review CF-9).
    pub(in crate::pages) fn retry(self) {
        let holds = self.env.rescue.with_untracked(Rescue::holds_page);
        if self.open_id().is_none() && !holds {
            self.first_load();
        } else {
            self.env.refresh.run(());
        }
    }

    /// New chat on the picker's model, opened once made unless the owner chose
    /// something else meanwhile. `kind` is "chat" | "admin", or "temporary" for a
    /// chat that is never saved.
    pub(in crate::pages) fn new_thread(self, kind: &'static str) {
        let env = self.env;
        let since = self.starting();
        let alias = env.model_sel.get_untracked();
        let body = if kind == "temporary" {
            json!({ "model_alias": alias, "kind": "chat", "temporary": true })
        } else {
            json!({ "model_alias": alias, "kind": kind })
        };
        spawn_local(async move {
            match crate::api::post::<ChatThread, _>("/chat/api/threads", &body).await {
                Ok(t) => {
                    let id = t.id;
                    // The page was left before the answer: nothing will show
                    // the chat, and a temporary one would never be discarded.
                    if !env.scope.alive() {
                        if id < 0 {
                            chat_temp::discard(id);
                        }
                        return;
                    }
                    // Not opened, the owner having gone elsewhere: a
                    // temporary chat no one saw is thrown away, as leaving
                    // one does; a stored one stays, listed and empty.
                    if self.follow(since, id) || id > 0 {
                        env.refresh.run(());
                    } else {
                        chat_temp::discard(id);
                    }
                }
                Err(e) => {
                    let back = self.failed(since);
                    env.toasts.err(format!("{e}{back}"));
                }
            }
        });
    }

    /// The open thread `id` is gone, deleted here or elsewhere (`since`: as
    /// the delete started, [`Self::now`]): the next one opens (the old app's
    /// rule), and with none left the URL goes back to bare /chat. Not over
    /// an open started since, or one still out (review CF-4): that one is
    /// where the page goes.
    pub(in crate::pages) fn fall_through(self, id: i64, since: Since) {
        let env = self.env;
        if self.open_id() != Some(id) {
            return;
        }
        env.own.bump();
        env.current.set(None);
        env.msgs.set(Vec::new());
        let falls = self
            .opens
            .try_with_value(|o| o.falls_through(since))
            .unwrap_or(false);
        let next = env
            .threads
            .with_untracked(|v| v.iter().map(|t| t.id).find(|t| *t != id));
        match next.filter(|_| falls) {
            Some(n) => self.open_with(n, Why::Show),
            None => {
                env.stats.set(None);
                sync_url(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: i64) -> ChatThread {
        ChatThread {
            id,
            ..Default::default()
        }
    }

    #[test]
    fn deep_link_opens_the_named_thread_when_it_exists() {
        let list = vec![thread(7), thread(3)];
        assert_eq!(seed_thread(Some(3), &list), Some(3));
    }

    #[test]
    fn deep_link_opens_the_named_thread_even_off_the_active_list() {
        // Not in the (active-only) list a cold load fetches — an archived
        // thread, or one deleted since — still wins over the first active
        // thread (finding #7).
        let list = vec![thread(7), thread(3)];
        assert_eq!(seed_thread(Some(99), &list), Some(99));
    }

    #[test]
    fn no_deep_link_falls_back_to_the_first_thread() {
        let list = vec![thread(7), thread(3)];
        assert_eq!(seed_thread(None, &list), Some(7));
        assert_eq!(seed_thread(Some(3), &[]), Some(3));
        assert_eq!(seed_thread(None, &[]), None);
    }
}
