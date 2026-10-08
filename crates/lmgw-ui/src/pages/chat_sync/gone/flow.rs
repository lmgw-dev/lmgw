//! The rescued draft's new chat on plain values, from the delete elsewhere
//! to the send (reviews CL-14, CL-19 to CL-22, CF-1 to CF-3, CF-6). `gone`
//! applies each step to the page.
//!
//! - **The delete makes nothing** ([`on_gone`]): the draft waits, or the
//!   page falls through. There is no third answer, and no request:
//!   lmgw-core's `tests/it/chat_page_gone_scan.rs` reads these files and
//!   refuses one outside the Send's own functions (review CF-7).
//! - **One Send at a time** ([`Rescue::send`]): from the create to the send
//!   itself, a second Send is refused, so a double Enter neither makes nor
//!   opens the chat twice (CL-19).
//! - **Left is left** ([`Rescue::leave`]): another open or a New chat drops
//!   the waiting chat as it *starts*, and with it the Send under way. A
//!   create that answers afterwards neither opens nor sends; the chat it
//!   made stays, empty, as the New chat button leaves one (CL-20).
//! - **Unless the leaving fails** ([`Rescue::restore`]): the open or New
//!   chat that left it failed, and nothing else shows. The chat waits
//!   again, the one a Send under way made included (CF-1).
//! - **Looked at before it is sent into** ([`Rescue::opened`]): the open is
//!   the read. A chat another writer has written in meanwhile keeps the
//!   draft in the composer, and says so of a model the owner picked (CL-19,
//!   CF-3).
//! - **On the model picked** ([`Step::SetModel`]): a folder that names a
//!   model makes the chat on it. When the owner picked another while the
//!   draft waited, the chat's model is set to that pick before the send
//!   (CL-21); what the picker showed untouched is no pick (CF-3).
//! - **Not in an ongoing folder** ([`super::held`]): the chat Send makes
//!   there is the folder's current thread, the conversation a device is
//!   bound to, on the folder's model. The picker is held on it, and no
//!   model is written (CF-2).

use super::super::super::chat::ThreadDetail;
use super::super::super::chat_folders::FolderInfo;
use super::{folder_model, held, how, Make, NewChat};

/// What the page does when the open thread was deleted elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::pages) enum OnGone {
    /// No draft: the next thread opens, as after the page's own delete.
    FallThrough,
    /// The draft waits for a new chat in `folder`, made only by Send. The
    /// picker starts on `model`, the folder's, when it names one.
    Wait {
        folder: Option<i64>,
        model: Option<String>,
    },
}

/// What the page does when the open thread, in `folder`, was deleted
/// elsewhere, the composer holding a draft (`drafted`) or not.
pub(in crate::pages) fn on_gone(
    drafted: bool,
    folder: Option<i64>,
    folders: &[FolderInfo],
) -> OnGone {
    if !drafted {
        return OnGone::FallThrough;
    }
    let folder = super::rescue_folder(folder, folders);
    let model = folder
        .and_then(|f| folders.iter().find(|x| x.id == f))
        .and_then(folder_model);
    OnGone::Wait { folder, model }
}

/// The Send under way: its ticket, and the model the owner picked while the
/// draft waited, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sending {
    ticket: u64,
    picked: Option<String>,
}

/// The rescued draft's state: the new chat it waits in, and the Send
/// making it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::pages) struct Rescue {
    /// The new chat the draft waits in; `None` when none does.
    pub waiting: Option<NewChat>,
    sending: Option<Sending>,
    /// Tickets handed out so far.
    sends: u64,
    /// The waiting chat, left for an open or a New chat that has not shown
    /// yet: the page keeps the composer up meanwhile, without the strip, and
    /// puts it back if that fails (review CF-1).
    leaving: Option<NewChat>,
    /// The ticket of a Send whose chat is still being made, even if the Send
    /// was left meanwhile: the next Send waits for it, so a leaving that
    /// fails before the create answers does not make a second chat.
    creating: Option<u64>,
    /// The model the picker showed as the draft began to wait: a picker
    /// that still shows it at Send was not touched (review CF-3).
    start: String,
}

/// How the rescued draft's open landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::pages) enum Opened {
    /// Thread `id` shows, with this many messages, on this model.
    Shown {
        id: i64,
        messages: usize,
        model: String,
    },
    /// The read failed; `gone` when the chat is no longer there.
    Failed { gone: bool, error: String },
    /// Another open started after it: it was dropped, never shown.
    Superseded,
}

impl Opened {
    /// How an open's read landed (review CF-8): the thread as read, or the
    /// failure, a 404 and only a 404 saying it is gone.
    pub(in crate::pages) fn of(got: &Result<ThreadDetail, crate::api::Error>) -> Opened {
        match got {
            Ok(d) => Opened::Shown {
                id: d.thread.id,
                messages: d.messages.len(),
                model: d.thread.model_alias.clone(),
            },
            Err(e) => Opened::Failed {
                gone: e.is_not_found(),
                error: e.to_string(),
            },
        }
    }
}

/// What the page does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::pages) enum Step {
    /// The Send was left, or is not the one under way: nothing.
    Left,
    /// Send the draft: the page's ordinary send.
    Send,
    /// Set chat `id`'s model from `was` to the owner's pick, then
    /// [`Rescue::model_set`].
    SetModel { id: i64, pick: String, was: String },
    /// Another writer has written in the chat: it is open, and the draft
    /// stays in the composer for the owner to send there or not. The model
    /// the owner picked, `dropped`, was not set (review CF-3).
    Written { dropped: Option<String> },
    /// The chat's model could not be set: it is open on its own, and the
    /// draft stays in the composer.
    ModelRefused,
    /// The open failed: the draft waits, and the next Send opens the chat
    /// again, or makes another when it is gone.
    Failed,
}

impl Rescue {
    /// The open thread, in `folder`, was deleted elsewhere under a draft:
    /// it waits, the picker on `model`. Nothing is made.
    pub(in crate::pages) fn wait(&mut self, folder: Option<i64>, model: &str) {
        self.waiting = Some(NewChat { folder, made: None });
        self.sending = None;
        self.leaving = None;
        // An older Send's create answering now belongs to the draft before.
        self.creating = None;
        self.start = model.to_string();
    }

    /// Another open or a New chat starts: the waiting chat is dropped as an
    /// unsent draft goes, and a Send under way is void from here on.
    pub(in crate::pages) fn leave(&mut self) {
        if let Some(w) = self.waiting.take() {
            self.leaving = Some(w);
        }
        self.sending = None;
    }

    /// The open or New chat that left the waiting chat failed, and nothing
    /// else is on its way to show: the chat waits again, with no Send under
    /// way (review CF-1). Whether it does.
    pub(in crate::pages) fn restore(&mut self) -> bool {
        let Some(w) = self.leaving.take() else {
            return false;
        };
        self.waiting = Some(w);
        self.sending = None;
        true
    }

    /// A thread shows: nothing waits any more, and nothing is left.
    pub(in crate::pages) fn shown(&mut self) {
        self.waiting = None;
        self.leaving = None;
    }

    /// The page keeps the conversation pane up: a chat waits, or the open
    /// that left one has not shown yet.
    pub(in crate::pages) fn holds_page(&self) -> bool {
        self.waiting.is_some() || self.leaving.is_some()
    }

    /// The waiting chat was left for an open or a New chat that has not
    /// shown yet.
    pub(in crate::pages) fn opening(&self) -> bool {
        self.leaving.is_some()
    }

    /// Nothing waits, and no Send is under way.
    #[cfg(test)]
    fn is_idle(&self) -> bool {
        self.waiting.is_none() && self.sending.is_none()
    }

    /// A Send is under way: another waits for it.
    pub(in crate::pages) fn making(&self) -> bool {
        self.sending.is_some()
    }

    /// Send, the picker on `picker`: the Send's ticket and how it makes the
    /// chat ([`how`]). `None` while nothing waits or a Send is under way.
    /// The picker is the owner's pick only when it moved off the model it
    /// started on (review CF-3), and never in an ongoing folder, which holds
    /// it on its own (review CF-2).
    pub(in crate::pages) fn send(
        &mut self,
        folders: &[FolderInfo],
        picker: &str,
    ) -> Option<(u64, Make)> {
        let p = self.waiting?;
        if self.sending.is_some() || self.creating.is_some() {
            return None;
        }
        let picked =
            (held(p.folder, folders).is_none() && !picker.is_empty() && picker != self.start)
                .then(|| picker.to_string());
        self.sends += 1;
        self.creating = Some(self.sends);
        self.sending = Some(Sending {
            ticket: self.sends,
            picked,
        });
        Some((self.sends, how(p, folders)))
    }

    fn holds(&self, ticket: u64) -> bool {
        self.sending.as_ref().is_some_and(|s| s.ticket == ticket)
    }

    /// Send `ticket`'s chat is made, thread `id`: whether it is opened. Not
    /// once the Send was left: the chat stays, empty and listed, and should
    /// the leaving fail, the chat that waits again is this one.
    pub(in crate::pages) fn made(&mut self, ticket: u64, id: i64) -> bool {
        let mine = self.creating == Some(ticket);
        if mine {
            self.creating = None;
        }
        if !self.holds(ticket) {
            if ticket == self.sends {
                if let Some(w) = self.leaving.as_mut() {
                    w.made = Some(id);
                } else if let Some(w) = self.waiting.as_mut().filter(|_| mine) {
                    // The leaving failed before this answered: the chat
                    // that waits again is the one that got this chat.
                    w.made = Some(id);
                }
            }
            return false;
        }
        if let Some(w) = self.waiting.as_mut() {
            w.made = Some(id);
        }
        true
    }

    /// Making Send `ticket`'s chat failed: the draft waits, and the next
    /// Send tries again. Whether the Send still held (so it is said).
    pub(in crate::pages) fn failed(&mut self, ticket: u64) -> bool {
        if self.creating == Some(ticket) {
            self.creating = None;
        }
        if !self.holds(ticket) {
            return false;
        }
        self.sending = None;
        true
    }

    /// Send `ticket`'s open landed as `o`.
    pub(in crate::pages) fn opened(&mut self, ticket: u64, o: &Opened) -> Step {
        if !self.holds(ticket) {
            return Step::Left;
        }
        let picked = self.sending.take().and_then(|s| s.picked);
        match o {
            Opened::Superseded => Step::Left,
            Opened::Failed { gone, .. } => {
                if *gone {
                    if let Some(w) = self.waiting.as_mut() {
                        w.made = None;
                    }
                }
                Step::Failed
            }
            Opened::Shown {
                id,
                messages,
                model,
            } => {
                // It is open: nothing waits any more.
                self.waiting = None;
                let pick = picked.filter(|p| p != model);
                if *messages > 0 {
                    return Step::Written { dropped: pick };
                }
                let Some(pick) = pick else {
                    return Step::Send;
                };
                self.sending = Some(Sending {
                    ticket,
                    picked: Some(pick.clone()),
                });
                Step::SetModel {
                    id: *id,
                    pick,
                    was: model.clone(),
                }
            }
        }
    }

    /// Send `ticket`'s model was set (`ok`) or refused.
    pub(in crate::pages) fn model_set(&mut self, ticket: u64, ok: bool) -> Step {
        if !self.holds(ticket) {
            return Step::Left;
        }
        self.sending = None;
        if ok {
            Step::Send
        } else {
            Step::ModelRefused
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_folders::FolderOngoing;
    use serde_json::json;

    fn folder(id: i64, ongoing: bool, model: Option<&str>) -> FolderInfo {
        FolderInfo {
            id,
            name: format!("f{id}"),
            ongoing: ongoing.then(FolderOngoing::default),
            defaults: model.map_or(json!({}), |m| json!({ "model_alias": m })),
            ..Default::default()
        }
    }

    /// Chat 7 shows with `messages` messages, on `model`.
    fn shown(messages: usize, model: &str) -> Opened {
        Opened::Shown {
            id: 7,
            messages,
            model: model.into(),
        }
    }

    /// Ongoing folder 1 and plain folder 2, both on model `m`.
    fn folders() -> Vec<FolderInfo> {
        vec![folder(1, true, Some("m")), folder(2, false, Some("m"))]
    }

    /// A draft waiting in folder `f`, the picker on `m`.
    fn waiting_in(f: i64) -> (Rescue, Vec<FolderInfo>) {
        let mut r = Rescue::default();
        r.wait(Some(f), "m");
        (r, folders())
    }

    /// A draft waiting in ongoing folder 1, whose model is `m`.
    fn waiting() -> (Rescue, Vec<FolderInfo>) {
        waiting_in(1)
    }

    /// Review CL-14, CL-22: the delete elsewhere makes nothing. A draft
    /// waits, and only a Send hands out a create.
    #[test]
    fn the_delete_elsewhere_makes_nothing_a_draft_waits_for_send() {
        let folders = vec![folder(1, true, Some("m")), folder(2, false, None)];
        assert_eq!(on_gone(false, Some(1), &folders), OnGone::FallThrough);
        assert_eq!(
            on_gone(true, Some(1), &folders),
            OnGone::Wait {
                folder: Some(1),
                model: Some("m".into())
            }
        );
        assert_eq!(
            on_gone(true, Some(2), &folders),
            OnGone::Wait {
                folder: Some(2),
                model: None
            },
            "a folder that names no model leaves the picker"
        );
        assert_eq!(
            on_gone(true, Some(9), &folders),
            OnGone::Wait {
                folder: None,
                model: None
            },
            "a folder gone too"
        );
        let mut r = Rescue::default();
        r.wait(Some(1), "m");
        assert_eq!(
            r.waiting,
            Some(NewChat {
                folder: Some(1),
                made: None
            })
        );
        assert!(!r.making(), "nothing is under way until Send");
        assert_eq!(r.send(&folders, "m"), Some((1, Make::Conversation(1))));
        assert!(r.making());
    }

    /// Review CF-8: an open lands as the thread it read, or as its failure,
    /// and only a 404 says the chat is gone.
    #[test]
    fn an_open_lands_as_the_thread_read_and_only_a_404_is_gone() {
        use super::super::super::super::chat::{ChatThread, MsgRow};
        let read = ThreadDetail {
            thread: ChatThread {
                id: 7,
                model_alias: "m".into(),
                ..Default::default()
            },
            messages: vec![MsgRow::default(), MsgRow::default()],
            ..Default::default()
        };
        assert_eq!(Opened::of(&Ok(read)), shown(2, "m"));
        let api = |code: &str| {
            crate::api::Error::Api(lmgw_api_types::ApiError {
                code: code.into(),
                message: format!("the {code} answer"),
            })
        };
        assert_eq!(
            Opened::of(&Err(api("not_found"))),
            Opened::Failed {
                gone: true,
                error: "the not_found answer".into()
            }
        );
        assert_eq!(
            Opened::of(&Err(api("internal"))),
            Opened::Failed {
                gone: false,
                error: "the internal answer".into()
            }
        );
        assert_eq!(
            Opened::of(&Err(crate::api::Error::Transport("reset".into()))),
            Opened::Failed {
                gone: false,
                error: "connection failed: reset".into()
            },
            "a connection that failed is no delete"
        );
    }

    /// Review CL-19: from the create to the send, a second Send is refused,
    /// so the chat is neither made nor opened twice.
    #[test]
    fn a_second_send_waits_for_the_first_until_it_sent() {
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        assert_eq!(r.send(&folders, "m"), None, "while the create is out");
        assert!(r.made(t, 7));
        assert_eq!(r.send(&folders, "m"), None, "while the open is out");
        assert_eq!(r.opened(t, &shown(0, "m")), Step::Send);
        assert!(!r.making() && r.waiting.is_none());
        assert_eq!(r.send(&folders, "m"), None, "it is open: an ordinary send");
    }

    /// Review CL-19: a failed open keeps the made chat; the next Send opens
    /// it again, never makes another, and sends only into an empty one. A
    /// chat gone meanwhile is made afresh.
    #[test]
    fn a_retry_reads_the_made_chat_and_keeps_the_draft_once_written_in() {
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        assert!(r.made(t, 7));
        let failed = Opened::Failed {
            gone: false,
            error: "500".into(),
        };
        assert_eq!(r.opened(t, &failed), Step::Failed);
        assert!(!r.making(), "the next Send may try again");
        let (t2, how) = r.send(&folders, "m").unwrap();
        assert_eq!(how, Make::Open(7), "opened again, not made again");
        assert_eq!(
            r.opened(t2, &shown(2, "m")),
            Step::Written { dropped: None }
        );
        assert!(r.is_idle(), "it is open, the draft in the composer");

        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        assert!(r.made(t, 7));
        let gone = Opened::Failed {
            gone: true,
            error: "404".into(),
        };
        assert_eq!(r.opened(t, &gone), Step::Failed);
        let (_, how) = r.send(&folders, "m").unwrap();
        assert_eq!(how, Make::Conversation(1), "gone: made afresh");
    }

    /// Review CL-20: another open or a New chat started while Send makes
    /// the chat drops it then. The create's answer opens nothing and sends
    /// nothing, and a Send in the next chat is not held up by it.
    #[test]
    fn leaving_while_send_makes_the_chat_opens_and_sends_nothing() {
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        r.leave();
        assert!(r.is_idle() && !r.making());
        assert!(r.holds_page(), "the pane stays up until the next shows");
        assert!(
            !r.made(t, 7),
            "the create answers: the chat stays, unopened"
        );
        assert_eq!(r.opened(t, &shown(0, "m")), Step::Left);
        assert!(!r.failed(t), "a failure after leaving is not said");
        r.shown();
        assert!(!r.holds_page());
        r.leave();
        assert!(!r.holds_page(), "nothing waited: nothing to hold");

        // Left while its open was out: it is dropped as it lands.
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        assert!(r.made(t, 7));
        r.leave();
        assert_eq!(r.opened(t, &shown(0, "m")), Step::Left);
        assert_eq!(r.opened(t, &Opened::Superseded), Step::Left);

        // Left while its model was being set: never sent.
        let (mut r, folders) = waiting_in(2);
        let (t, _) = r.send(&folders, "n").unwrap();
        assert!(r.made(t, 7));
        assert!(matches!(r.opened(t, &shown(0, "m")), Step::SetModel { .. }));
        r.leave();
        assert_eq!(r.model_set(t, true), Step::Left);

        // A draft deleted under again, while an old create is out: that
        // create does not take the new wait.
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        r.leave();
        r.wait(Some(1), "m");
        assert!(!r.made(t, 7));
        assert_eq!(r.waiting.and_then(|w| w.made), None);
        assert!(r.send(&folders, "m").is_some(), "its own Send goes");
    }

    /// Review CF-1: the open or New chat that left the waiting chat failed.
    /// It waits again, nothing under way, and the next Send makes it, or
    /// opens the chat a Send under way made meanwhile.
    #[test]
    fn a_leaving_that_fails_puts_the_waiting_chat_back() {
        let (mut r, folders) = waiting();
        r.leave();
        assert!(r.holds_page() && r.opening() && r.waiting.is_none());
        assert!(r.restore());
        assert_eq!(
            r.waiting,
            Some(NewChat {
                folder: Some(1),
                made: None
            })
        );
        assert!(!r.opening() && !r.making() && r.holds_page());
        assert_eq!(r.send(&folders, "m"), Some((1, Make::Conversation(1))));

        // Left while its Send made the chat: back, the next Send opens that
        // chat rather than making a second one.
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        r.leave();
        assert!(!r.made(t, 7), "nothing opens while it is left");
        assert!(r.restore());
        assert!(!r.making(), "the Send left is not under way again");
        assert_eq!(r.opened(t, &shown(0, "m")), Step::Left, "nor is its open");
        let (_, how) = r.send(&folders, "m").unwrap();
        assert_eq!(how, Make::Open(7));

        // Left while its Send is making the chat, and the leaving fails
        // before the create answers: a second Send waits for the create and
        // then opens that chat, it makes no second one.
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        r.leave();
        assert!(r.restore());
        assert!(r.send(&folders, "m").is_none(), "the create is still out");
        assert!(!r.made(t, 7), "the Send was left: nothing opens");
        assert_eq!(r.waiting.and_then(|w| w.made), Some(7));
        let (_, how) = r.send(&folders, "m").unwrap();
        assert_eq!(how, Make::Open(7));

        // A create that failed frees the next Send.
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "m").unwrap();
        r.leave();
        assert!(r.restore());
        assert!(!r.failed(t));
        assert!(r.send(&folders, "m").is_some());

        // A second leave keeps the chat the first one left.
        let (mut r, _) = waiting();
        r.leave();
        r.leave();
        assert!(r.restore() && r.waiting.is_some());

        // Shown, nothing comes back; nothing waited, nothing to put back.
        let (mut r, _) = waiting();
        r.leave();
        r.shown();
        assert!(!r.restore() && r.waiting.is_none() && !r.holds_page());
        let mut r = Rescue::default();
        r.leave();
        assert!(!r.restore() && r.is_idle() && !r.holds_page());

        // Deleted under again meanwhile: the new wait is the one.
        let (mut r, _) = waiting();
        r.leave();
        r.wait(Some(2), "m");
        assert!(!r.restore());
        assert_eq!(r.waiting.map(|w| w.folder), Some(Some(2)));
    }

    /// Review CL-21: in a plain folder that names a model, a model the
    /// owner picked while the draft waited is set on the chat before the
    /// send; a refused one keeps the draft. So is it with no folder.
    #[test]
    fn the_owner_s_pick_is_the_chat_s_model_before_the_send() {
        let (mut r, folders) = waiting_in(2);
        let (t, how) = r.send(&folders, "n").unwrap();
        assert_eq!(how, Make::InFolder(2));
        assert!(r.made(t, 7));
        assert_eq!(
            r.opened(t, &shown(0, "m")),
            Step::SetModel {
                id: 7,
                pick: "n".into(),
                was: "m".into()
            }
        );
        assert!(r.making(), "a Send waits while the model is set");
        assert_eq!(r.model_set(t, true), Step::Send);
        assert!(r.is_idle());

        let (mut r, folders) = waiting_in(2);
        let (t, _) = r.send(&folders, "n").unwrap();
        assert!(r.made(t, 7));
        assert!(matches!(r.opened(t, &shown(0, "m")), Step::SetModel { .. }));
        assert_eq!(r.model_set(t, false), Step::ModelRefused);
        assert!(r.is_idle());

        // No folder: made on the picker's model, so no write is needed; a
        // chat made on another all the same takes the pick.
        let mut r = Rescue::default();
        r.wait(None, "m");
        let (t, how) = r.send(&folders, "n").unwrap();
        assert_eq!(how, Make::Plain);
        assert!(r.made(t, 7));
        assert_eq!(r.opened(t, &shown(0, "n")), Step::Send);
    }

    /// Review CF-3: only a pick is written. A picker the owner did not
    /// touch, or moved back to where it started, writes nothing, even when
    /// the chat was made on another model (the folder's changed since, say).
    #[test]
    fn a_picker_the_owner_did_not_move_writes_no_model() {
        for picker in ["m", ""] {
            let (mut r, folders) = waiting_in(2);
            let (t, _) = r.send(&folders, picker).unwrap();
            assert!(r.made(t, 7));
            assert_eq!(r.opened(t, &shown(0, "m2")), Step::Send, "{picker:?}");
        }
        // A pick another writer's turn came before: said, not set.
        let (mut r, folders) = waiting_in(2);
        let (t, _) = r.send(&folders, "n").unwrap();
        assert!(r.made(t, 7));
        assert_eq!(
            r.opened(t, &shown(1, "m")),
            Step::Written {
                dropped: Some("n".into())
            }
        );
        // The chat is on the pick already: nothing dropped, nothing set.
        let (mut r, folders) = waiting_in(2);
        let (t, _) = r.send(&folders, "n").unwrap();
        assert!(r.made(t, 7));
        assert_eq!(r.opened(t, &shown(1, "n")), Step::Written { dropped: None });
        let (mut r, folders) = waiting_in(2);
        let (t, _) = r.send(&folders, "n").unwrap();
        assert!(r.made(t, 7));
        assert_eq!(r.opened(t, &shown(0, "n")), Step::Send);
    }

    /// Review CF-2: in an ongoing folder the chat Send makes is the folder's
    /// current thread, a device's conversation, on the folder's model. The
    /// picker is held there and no model is written, whatever it shows; a
    /// retry of a made chat in that folder writes none either.
    #[test]
    fn in_an_ongoing_folder_no_model_is_written() {
        for picker in ["m", "n"] {
            let (mut r, folders) = waiting();
            let (t, how) = r.send(&folders, picker).unwrap();
            assert_eq!(how, Make::Conversation(1));
            assert!(r.made(t, 7));
            assert_eq!(r.opened(t, &shown(0, "m")), Step::Send, "{picker:?}");
            assert!(r.is_idle());
        }
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "n").unwrap();
        assert!(r.made(t, 7));
        assert_eq!(
            r.opened(t, &shown(1, "m")),
            Step::Written { dropped: None },
            "no pick to drop"
        );
        let (mut r, folders) = waiting();
        let (t, _) = r.send(&folders, "n").unwrap();
        assert!(r.made(t, 7));
        let failed = Opened::Failed {
            gone: false,
            error: "500".into(),
        };
        assert_eq!(r.opened(t, &failed), Step::Failed);
        let (t, how) = r.send(&folders, "n").unwrap();
        assert_eq!(how, Make::Open(7));
        assert_eq!(r.opened(t, &shown(0, "m")), Step::Send);

        // A plain folder that became ongoing while the draft waited holds it
        // too: what the folder is at Send decides.
        let (mut r, mut folders) = waiting_in(2);
        folders[1].ongoing = Some(FolderOngoing::default());
        let (t, how) = r.send(&folders, "n").unwrap();
        assert_eq!(how, Make::Conversation(2));
        assert!(r.made(t, 7));
        assert_eq!(r.opened(t, &shown(0, "m")), Step::Send);
    }
}
