//! A draft whose conversation was deleted elsewhere (reviews CL-2, CL-14,
//! CL-19 to CL-21): it never goes into another conversation, and nothing
//! is made for it until the owner presses Send. The steps are decided on
//! plain values in [`flow`].
//!
//! - **Without a draft** the page falls through to the next thread, as
//!   its own delete does.
//! - **With one** (text, files or knowledge picks in the composer) the page
//!   holds an unsaved new chat ([`NewChat`]): an empty transcript, the
//!   draft, its chips and picks, a strip that says it, and a toast. Its
//!   folder is the deleted conversation's while the list still has it.
//!   The files went with the deleted conversation: their chips say so, and
//!   a Send leaves them out. Files attach once the chat is made.
//! - **Send** makes it exactly as the page's own New chat in that folder
//!   does ([`how`]): an ongoing folder's "New conversation" (the server's
//!   ordinary rule: a new current thread, or the current one while it has
//!   no message), a plain folder's "New chat here", or a New chat with no
//!   folder. Then it opens with the draft kept, which reads it: a chat
//!   another writer has written in meanwhile keeps the draft in the
//!   composer. A folder that names a model makes the chat on it; a model
//!   the owner picked meanwhile is then set on the chat. Then the draft is
//!   sent. So an ongoing folder's current thread, which another client may
//!   be talking in, moves only at the owner's Send. Until the draft is sent
//!   another Send waits, and the model picker is held.
//! - **An ongoing folder's model is the folder's** ([`held`], review CF-2):
//!   the chat Send makes there is the folder's current thread, the
//!   conversation a device is bound to. While the draft waits in one, the
//!   picker is held on the folder's model, and the strip says why; Send
//!   writes no model.
//! - **Leaving it** (opening a thread, a New chat, leaving the page) drops
//!   it as an ordinary unsent draft goes, as the leaving starts: nothing
//!   was stored, or a Send under way made a chat that stays, empty, as the
//!   New chat button leaves one, and is neither opened nor sent into. An
//!   open or a New chat that left it and failed, nothing else on its way to
//!   show, puts it back: the strip, and Send makes it (review CF-1).

mod flow;
mod landing;

use leptos::prelude::*;
use serde_json::json;

use super::super::chat::{sync_url, ChatThread, Msg, Stats};
use super::super::chat_attach::DraftChip;
use super::super::chat_folders::{
    start_chat_in, start_conversation, FolderEnv, FolderInfo, CONTINUING,
};
use super::OwnEdits;
use crate::scope::Scope;
use crate::widgets::Toasts;
use flow::{on_gone, OnGone};
pub(in crate::pages) use flow::{Opened, Rescue};
pub(in crate::pages) use landing::{rescued_open, Landing};

/// A new chat the composer's draft waits in, not stored until Send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::pages) struct NewChat {
    /// The folder it is made in; `None` for none.
    pub folder: Option<i64>,
    /// Made by a Send whose open did not land: the next Send opens this
    /// one rather than making another.
    pub made: Option<i64>,
}

/// What the page hands the delete-elsewhere path. `Copy`.
#[derive(Clone, Copy)]
pub(in crate::pages) struct Gone {
    pub current: RwSignal<Option<ChatThread>>,
    pub msgs: RwSignal<Vec<Msg>>,
    pub stats: RwSignal<Option<(i64, Stats)>>,
    pub composer: RwSignal<String>,
    pub chips: RwSignal<Vec<DraftChip>>,
    /// The composer's knowledge picks.
    pub kbs: RwSignal<Vec<i64>>,
    pub folders: RwSignal<Vec<FolderInfo>>,
    pub model_sel: RwSignal<String>,
    pub rescue: RwSignal<Rescue>,
    pub own: OwnEdits,
    pub toasts: Toasts,
    /// The page's delete path: the next thread opens.
    pub fall_through: Callback<i64>,
}

impl Gone {
    /// The open thread `id` was deleted elsewhere (module doc). It decides
    /// through [`on_gone`] and sets the page's signals: no request.
    pub(in crate::pages) fn run(self, id: i64) {
        let Some(gone) = self.current.get_untracked().filter(|t| t.id == id) else {
            return;
        };
        let is_drafted = drafted(
            &self.composer.get_untracked(),
            self.chips.with_untracked(Vec::len),
            self.kbs.with_untracked(Vec::len),
        );
        let step = self
            .folders
            .with_untracked(|v| on_gone(is_drafted, gone.folder_id, v));
        let OnGone::Wait { folder, model } = step else {
            self.toasts
                .warn("the open conversation was deleted elsewhere");
            self.fall_through.run(id);
            return;
        };
        let lost = files_gone(self.chips);
        let name = folder.and_then(|f| {
            self.folders
                .with_untracked(|v| v.iter().find(|x| x.id == f).map(|x| x.name.clone()))
        });
        self.own.bump();
        self.current.set(None);
        self.msgs.set(Vec::new());
        self.stats.set(None);
        sync_url(None);
        // The model the new chat starts on: its folder's, when the folder
        // names one, as the folder's own New chat takes it. A model the
        // owner picks instead is set on the chat at Send (review CL-21),
        // outside an ongoing folder (CF-2).
        if let Some(m) = model {
            self.model_sel.set(m);
        }
        let start = self.model_sel.get_untracked();
        self.rescue.update(|r| r.wait(folder, &start));
        self.toasts.warn(draft_kept_note(lost, name.as_deref()));
    }
}

/// Whether the composer holds a draft: text, files or knowledge picks.
pub(in crate::pages) fn drafted(composer: &str, chips: usize, kbs: usize) -> bool {
    !composer.trim().is_empty() || chips > 0 || kbs > 0
}

/// The folder a rescued draft's new chat goes in: the deleted
/// conversation's, while the list still has it.
pub(in crate::pages) fn rescue_folder(folder: Option<i64>, folders: &[FolderInfo]) -> Option<i64> {
    folder.filter(|f| folders.iter().any(|x| x.id == *f))
}

/// The model a folder names for its chats, if any.
fn folder_model(f: &FolderInfo) -> Option<String> {
    f.defaults["model_alias"]
        .as_str()
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// The model the picker is held on while the draft waits in `folder`
/// (review CF-2): an ongoing folder's, "" should it name none. The chat
/// Send makes there is the folder's current thread, the conversation a
/// device is bound to, and its model is the folder's. `None` in a plain
/// folder or none: the picker is the owner's.
pub(in crate::pages) fn held(folder: Option<i64>, folders: &[FolderInfo]) -> Option<String> {
    let f = folders.iter().find(|x| Some(x.id) == folder)?;
    f.ongoing.as_ref()?;
    Some(folder_model(f).unwrap_or_default())
}

/// Why the picker is held (review CF-2).
pub(in crate::pages) const HELD: &str =
    "this folder's ongoing conversation uses its folder's model; change it in the folder settings";

/// The model the picker is held on now ([`held`]): while a draft waits in
/// an ongoing folder.
pub(in crate::pages) fn held_model(
    rescue: RwSignal<Rescue>,
    folders: RwSignal<Vec<FolderInfo>>,
) -> Memo<Option<String>> {
    Memo::new(move |_| {
        let folder = rescue.with(|r| r.waiting.map(|p| p.folder))?;
        folders.with(|v| held(folder, v))
    })
}

/// How a Send makes the new chat: as the page's own New chat in its
/// folder does, by what the folder is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::pages) enum Make {
    /// Made already by a Send whose open did not land: open it.
    Open(i64),
    /// An ongoing folder's "New conversation".
    Conversation(i64),
    /// A plain folder's "New chat here".
    InFolder(i64),
    /// New chat, in no folder: the folder is gone, or there was none.
    Plain,
}

pub(in crate::pages) fn how(p: NewChat, folders: &[FolderInfo]) -> Make {
    if let Some(id) = p.made {
        return Make::Open(id);
    }
    match p.folder.and_then(|f| folders.iter().find(|x| x.id == f)) {
        None => Make::Plain,
        Some(f) if f.ongoing.is_some() => Make::Conversation(f.id),
        Some(f) => Make::InFolder(f.id),
    }
}

/// What a Send made: the thread, the folder it is in, and what to say.
struct Made {
    id: i64,
    folder: Option<i64>,
    note: Option<&'static str>,
}

/// The page's own New chat in no folder, on `alias`.
async fn new_plain(alias: &str) -> Result<Made, crate::api::Error> {
    let body = json!({ "model_alias": alias, "kind": "chat" });
    let t = crate::api::post::<ChatThread, _>("/chat/api/threads", &body).await?;
    Ok(Made {
        id: t.id,
        folder: None,
        note: None,
    })
}

/// Make the new chat as `how` says. A folder gone since (a 404) makes it
/// in none, as the strip would say by then.
async fn make(how: Make, alias: &str) -> Result<Made, crate::api::Error> {
    match how {
        Make::Open(id) => Ok(Made {
            id,
            folder: None,
            note: None,
        }),
        Make::Conversation(f) => match start_conversation(f).await {
            Ok((id, rolled_over)) => Ok(Made {
                id,
                folder: Some(f),
                note: (!rolled_over).then_some(CONTINUING),
            }),
            Err(e) if e.is_not_found() => new_plain(alias).await,
            Err(e) => Err(e),
        },
        Make::InFolder(f) => match start_chat_in(f, alias).await {
            Ok(t) => Ok(Made {
                id: t.id,
                folder: Some(f),
                note: None,
            }),
            Err(e) if e.is_not_found() => new_plain(alias).await,
            Err(e) => Err(e),
        },
        Make::Plain => new_plain(alias).await,
    }
}

/// Apply `f` to the rescue, telling its readers only when it changed.
/// `None` once the page is gone.
fn step<U>(rescue: RwSignal<Rescue>, f: impl FnOnce(&mut Rescue) -> U) -> Option<U> {
    rescue.try_maybe_update(|r| {
        let before = r.clone();
        let u = f(r);
        (*r != before, u)
    })
}

/// What a Send on the new chat needs from the page. `Copy`.
#[derive(Clone, Copy)]
pub(in crate::pages) struct SendNew {
    pub rescue: RwSignal<Rescue>,
    pub folders: FolderEnv,
    pub model_sel: RwSignal<String>,
    pub scope: Scope,
    pub toasts: Toasts,
    /// Read the thread list again.
    pub refresh: Callback<()>,
    /// Open the made chat for a Send, by its ticket, with the composer's
    /// draft kept; [`rescued_open`] takes it from there.
    pub open: Callback<(i64, u64)>,
}

/// Send on the new chat: make it (module doc) and open it with the draft,
/// on the model the picker shows. Nothing while another Send is under way.
/// A failure keeps the new chat and the draft.
pub(in crate::pages) fn send_new(env: SendNew) {
    let alias = env.model_sel.get_untracked();
    let Some((ticket, how)) = env
        .folders
        .folders
        .with_untracked(|v| step(env.rescue, |r| r.send(v, &alias)))
        .flatten()
    else {
        return;
    };
    env.scope.spawn(async move {
        match make(how, &alias).await {
            Ok(m) => {
                if let Some(f) = m.folder {
                    env.folders.expand(f);
                }
                env.refresh.run(());
                // Left while it was being made (review CL-20): the owner is
                // where they went. The chat stays, listed and empty.
                if !step(env.rescue, |r| r.made(ticket, m.id)).unwrap_or(false) {
                    return;
                }
                if let Some(n) = m.note {
                    env.toasts.ok(n);
                }
                env.open.run((m.id, ticket));
            }
            Err(e) => {
                if step(env.rescue, |r| r.failed(ticket)).unwrap_or(false) {
                    env.toasts.err(format!(
                        "making the new chat failed: {e} — the draft is kept, and Send tries again"
                    ));
                }
            }
        }
    });
}

/// Another open or a New chat starts: the new chat is left (module doc).
pub(in crate::pages) fn leave(rescue: RwSignal<Rescue>) {
    step(rescue, Rescue::leave);
}

/// The open or New chat that left the new chat failed, and nothing else is
/// on its way to show: the new chat waits again (module doc). Whether it
/// does.
pub(in crate::pages) fn restore(rescue: RwSignal<Rescue>) -> bool {
    step(rescue, Rescue::restore).unwrap_or(false)
}

/// What a failure that put the new chat back adds to its toast.
pub(in crate::pages) const WAITS_AGAIN: &str = " — your draft waits for its new chat again";

/// What Send or an attach says while the open that left the new chat has
/// not shown yet.
pub(in crate::pages) const OPENING: &str = "the conversation is still opening — send once it shows";

/// The new chat ends when a thread opens, and loses its folder when the
/// folder goes (module doc). The picker follows the model it is held on
/// ([`held_model`]), which a folder's settings may change meanwhile.
pub(in crate::pages) fn watch(
    rescue: RwSignal<Rescue>,
    folders: RwSignal<Vec<FolderInfo>>,
    open: Memo<Option<i64>>,
    model_sel: RwSignal<String>,
    held: Memo<Option<String>>,
) {
    Effect::new(move |_| {
        if let Some(m) = held.get().filter(|m| !m.is_empty()) {
            if model_sel.with_untracked(|s| *s != m) {
                model_sel.set(m);
            }
        }
    });
    Effect::new(move |_| {
        if open.get().is_some() {
            step(rescue, Rescue::shown);
        }
    });
    Effect::new(move |_| {
        let Some(f) = rescue.with(|r| r.waiting.and_then(|p| p.folder)) else {
            return;
        };
        if folders.with(|v| rescue_folder(Some(f), v).is_none()) {
            step(rescue, |r| {
                if let Some(p) = r.waiting.as_mut() {
                    p.folder = None;
                }
            });
        }
    });
}

/// What the strip above the composer says while the new chat waits: in
/// `folder`, its picker `held` on the model an ongoing folder sets.
pub(in crate::pages) fn strip_text(folder: Option<&str>, held: Option<&str>) -> String {
    let mut text = match folder {
        Some(name) => format!("New chat in “{name}” · made when you send"),
        None => "New chat · made when you send".to_string(),
    };
    if let Some(m) = held {
        if !m.is_empty() {
            text.push_str(&format!(" · on {m}"));
        }
        text.push_str(&format!(": {HELD}"));
    }
    text
}

/// The strip above the composer while the new chat waits.
#[component]
pub(in crate::pages) fn NewChatStrip(
    rescue: RwSignal<Rescue>,
    folders: RwSignal<Vec<FolderInfo>>,
    held: Memo<Option<String>>,
) -> impl IntoView {
    move || {
        rescue.with(|r| r.waiting).map(|p| {
            let name = p.folder.and_then(|f| {
                folders.with(|v| v.iter().find(|x| x.id == f).map(|x| x.name.clone()))
            });
            let held = held.get();
            view! {
                <div class="archive-strip new-chat-strip" class:held=held.is_some()>
                    <span>{strip_text(name.as_deref(), held.as_deref())}</span>
                </div>
            }
        })
    }
}

/// What an attach says while the new chat waits: files attach to a stored
/// chat, and this one is made by Send.
pub(in crate::pages) const NO_FILES_YET: &str =
    "this new chat is made when you send: send the message first, then attach files to it";

/// What a chip whose file went with the deleted conversation says.
const FILE_GONE: &str =
    "its file went with the deleted conversation: attach it again once the new chat is made";

/// The chips whose files the server held, gone with their conversation:
/// each says so and holds no id, so a Send leaves it out and its ✕ deletes
/// nothing on the server. How many. A chip still uploading hears it from
/// its upload's answer.
fn files_gone(chips: RwSignal<Vec<DraftChip>>) -> usize {
    chips.with_untracked(|v| {
        let mut n = 0;
        for c in v {
            if c.id.get_untracked().is_some() && c.error.with_untracked(Option::is_none) {
                c.id.set(None);
                c.error.set(Some(FILE_GONE.into()));
                n += 1;
            }
        }
        n
    })
}

/// The toast as the new chat starts waiting: `lost` files went with the
/// deleted conversation; `folder` is the new chat's.
fn draft_kept_note(lost: usize, folder: Option<&str>) -> String {
    let mut note = "the open conversation was deleted elsewhere — your draft was kept, for a new \
                    chat"
        .to_string();
    if let Some(name) = folder {
        note.push_str(&format!(" in “{name}”"));
    }
    note.push_str(" that is made when you send");
    match lost {
        0 => {}
        1 => note.push_str(" (its attached file went with the conversation)"),
        n => note.push_str(&format!(
            " (its {n} attached files went with the conversation)"
        )),
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_folders::FolderOngoing;

    fn folder(id: i64, ongoing: bool) -> FolderInfo {
        FolderInfo {
            id,
            name: format!("f{id}"),
            ongoing: ongoing.then(FolderOngoing::default),
            ..Default::default()
        }
    }

    #[test]
    fn a_draft_is_text_files_or_knowledge_picks() {
        assert!(!drafted("", 0, 0));
        assert!(!drafted("  \n", 0, 0), "whitespace is no draft");
        assert!(drafted("a note", 0, 0));
        assert!(drafted("", 1, 0));
        assert!(drafted("", 0, 1));
    }

    /// Review CL-14: nothing is made at the delete. Send makes the new
    /// chat as the page's own New chat in its folder does: an ongoing
    /// folder's "New conversation", a plain folder's "New chat here", no
    /// folder once it is gone.
    #[test]
    fn send_makes_the_new_chat_as_the_folder_s_own_new_chat_does() {
        let folders = vec![folder(1, true), folder(2, false)];
        let at = |f: Option<i64>| {
            how(
                NewChat {
                    folder: f,
                    made: None,
                },
                &folders,
            )
        };
        assert_eq!(at(Some(1)), Make::Conversation(1));
        assert_eq!(at(Some(2)), Make::InFolder(2));
        assert_eq!(at(Some(3)), Make::Plain, "a folder gone since");
        assert_eq!(at(None), Make::Plain);
        // Made by a Send whose open did not land: opened, never made twice.
        assert_eq!(
            how(
                NewChat {
                    folder: Some(1),
                    made: Some(9)
                },
                &folders
            ),
            Make::Open(9)
        );
    }

    #[test]
    fn the_new_chat_s_folder_is_the_deleted_one_s_while_it_is_listed() {
        let folders = vec![folder(1, true)];
        assert_eq!(rescue_folder(Some(1), &folders), Some(1));
        assert_eq!(rescue_folder(Some(4), &folders), None);
        assert_eq!(rescue_folder(None, &folders), None);
    }

    #[test]
    fn the_note_and_the_strip_say_the_chat_is_made_when_you_send() {
        let none = draft_kept_note(0, Some("Kai desktop"));
        assert!(none.contains("deleted elsewhere") && none.contains("draft was kept"));
        assert!(none.contains("in “Kai desktop” that is made when you send"));
        assert!(!none.contains("attached"));
        assert!(draft_kept_note(1, None).contains("for a new chat that is made when you send"));
        assert!(draft_kept_note(1, None).contains("its attached file went"));
        assert!(draft_kept_note(3, None).contains("its 3 attached files went"));
        assert_eq!(
            strip_text(Some("Kai desktop"), None),
            "New chat in “Kai desktop” · made when you send"
        );
        assert_eq!(strip_text(None, None), "New chat · made when you send");
    }

    /// Review CF-2: a draft waiting in an ongoing folder holds the picker
    /// on the folder's model, and the strip says why; a plain folder or
    /// none leaves the picker to the owner.
    #[test]
    fn an_ongoing_folder_holds_the_picker_on_its_model_and_says_so() {
        let mut folders = vec![folder(1, true), folder(2, false), folder(3, true)];
        folders[0].defaults = serde_json::json!({ "model_alias": "fake-echo" });
        folders[1].defaults = serde_json::json!({ "model_alias": "fake-echo" });
        assert_eq!(held(Some(1), &folders), Some("fake-echo".into()));
        assert_eq!(
            held(Some(3), &folders),
            Some(String::new()),
            "no model named"
        );
        assert_eq!(held(Some(2), &folders), None, "a plain folder");
        assert_eq!(held(Some(9), &folders), None, "a folder gone");
        assert_eq!(held(None, &folders), None);
        assert_eq!(
            strip_text(Some("Kai desktop"), Some("fake-echo")),
            "New chat in “Kai desktop” · made when you send · on fake-echo: this folder's \
             ongoing conversation uses its folder's model; change it in the folder settings"
        );
        assert!(strip_text(Some("f3"), Some("")).ends_with(&format!("made when you send: {HELD}")));
    }
}
